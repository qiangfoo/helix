use std::{borrow::Cow, collections::HashMap, iter};

use anyhow::Result;
use arc_swap::access::DynAccess;
use helix_core::NATIVE_LINE_ENDING;

use crate::view::{
    clipboard::{ClipboardProvider, ClipboardType},
    Editor,
};

/// A key-value store for saving sets of values.
///
/// Each register corresponds to a `char`. Most chars can be used to store any set of
/// values but a few chars are "special registers". Special registers have unique
/// behaviors when read or written to:
///
/// * Black hole (`_`): all values read and written are discarded
/// * Selection indices (`#`): index number of each selection starting at 1
/// * Selection contents (`.`)
/// * Document path (`%`): filename of the current buffer
/// * System clipboard (`*`)
/// * Primary clipboard (`+`)
pub struct Registers {
    /// The mapping of register to values.
    /// Values are stored in reverse order when inserted with `Registers::write`.
    /// The order is reversed again in `Registers::read`. This allows us to
    /// efficiently prepend new values in `Registers::push`.
    inner: HashMap<char, Vec<String>>,
    clipboard_provider: Box<dyn DynAccess<ClipboardProvider>>,
    pub last_search_register: char,
}

impl Registers {
    pub fn new(clipboard_provider: Box<dyn DynAccess<ClipboardProvider>>) -> Self {
        Self {
            inner: Default::default(),
            clipboard_provider,
            last_search_register: '/',
        }
    }

    pub fn read<'a>(&'a self, name: char, editor: &'a Editor) -> Option<RegisterValues<'a>> {
        match name {
            '_' => Some(RegisterValues::new(iter::empty())),
            '#' => {
                let (view, doc) = current_ref!(editor);
                let selections = doc.selection(view.id).len();
                // ExactSizeIterator is implemented for Range<usize> but
                // not RangeInclusive<usize>.
                Some(RegisterValues::new(
                    (0..selections).map(|i| (i + 1).to_string().into()),
                ))
            }
            '.' => {
                let (view, doc) = current_ref!(editor);
                let text = doc.text().slice(..);
                Some(RegisterValues::new(doc.selection(view.id).fragments(text)))
            }
            '%' => {
                let path = doc!(editor).display_name();
                Some(RegisterValues::new(iter::once(path)))
            }
            '*' | '+' | _ => self
                .inner
                .get(&name)
                .map(|values| RegisterValues::new(values.iter().map(Cow::from).rev())),
        }
    }

    pub fn write(&mut self, name: char, mut values: Vec<String>) -> Result<()> {
        match name {
            '_' => Ok(()),
            '#' | '.' | '%' => Err(anyhow::anyhow!("Register {name} does not support writing")),
            '*' | '+' => {
                self.clipboard_provider.load().set_contents(
                    &values.join(NATIVE_LINE_ENDING.as_str()),
                    match name {
                        '+' => ClipboardType::Clipboard,
                        '*' => ClipboardType::Selection,
                        _ => unreachable!(),
                    },
                )?;
                values.reverse();
                self.inner.insert(name, values);
                Ok(())
            }
            _ => {
                values.reverse();
                self.inner.insert(name, values);
                Ok(())
            }
        }
    }

    pub fn push(&mut self, name: char, value: String) -> Result<()> {
        match name {
            '_' => Ok(()),
            '#' | '.' | '%' => Err(anyhow::anyhow!("Register {name} does not support pushing")),
            '*' | '+' => {
                // Collect existing values plus the new one, then delegate to write
                let mut values: Vec<String> = self
                    .inner
                    .get(&name)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .rev()
                    .collect();
                values.push(value);
                self.write(name, values)
            }
            _ => {
                self.inner.entry(name).or_default().push(value);
                Ok(())
            }
        }
    }

    pub fn first<'a>(&'a self, name: char, editor: &'a Editor) -> Option<Cow<'a, str>> {
        self.read(name, editor).and_then(|mut values| values.next())
    }

    pub fn last<'a>(&'a self, name: char, editor: &'a Editor) -> Option<Cow<'a, str>> {
        self.read(name, editor)
            .and_then(|mut values| values.next_back())
    }

    pub fn clear(&mut self) {
        self.inner.clear()
    }

    pub fn remove(&mut self, name: char) -> bool {
        match name {
            '_' | '#' | '.' | '%' => false,
            _ => self.inner.remove(&name).is_some(),
        }
    }

    pub fn clipboard_provider_name(&self) -> String {
        self.clipboard_provider.load().name().into_owned()
    }
}

// This is a wrapper of an iterator that is both double ended and exact size,
// and can return either owned or borrowed values. Regular registers can
// return borrowed values while some special registers need to return owned
// values.
pub struct RegisterValues<'a> {
    iter: Box<dyn DoubleEndedExactSizeIterator<Item = Cow<'a, str>> + 'a>,
}

impl<'a> RegisterValues<'a> {
    fn new(
        iter: impl DoubleEndedIterator<Item = Cow<'a, str>>
            + ExactSizeIterator<Item = Cow<'a, str>>
            + 'a,
    ) -> Self {
        Self {
            iter: Box::new(iter),
        }
    }
}

impl<'a> Iterator for RegisterValues<'a> {
    type Item = Cow<'a, str>;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.iter.size_hint()
    }
}

impl DoubleEndedIterator for RegisterValues<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.iter.next_back()
    }
}

impl ExactSizeIterator for RegisterValues<'_> {
    fn len(&self) -> usize {
        self.iter.len()
    }
}

// Each RegisterValues iterator is both double ended and exact size. We can't
// type RegisterValues as `Box<dyn DoubleEndedIterator + ExactSizeIterator>`
// because only one non-auto trait is allowed in trait objects. So we need to
// create a new trait that covers both. `RegisterValues` wraps that type so that
// trait only needs to live in this module and not be imported for all register
// callsites.
trait DoubleEndedExactSizeIterator: DoubleEndedIterator + ExactSizeIterator {}

impl<I: DoubleEndedIterator + ExactSizeIterator> DoubleEndedExactSizeIterator for I {}
