//! These are macros to make getting very nested fields easier.
//! These are macros instead of functions because functions will have to take `&mut self`
//! However, rust doesn't know that you only want a partial borrow instead of borrowing the
//! entire struct which `&mut self` says.  This makes it impossible to do other mutable
//! stuff to the struct because it is already borrowed. Because macros are expanded,
//! this circumvents the problem because it is just like indexing fields by hand and then
//! putting a `&mut` in front of it. This way rust can see that we are only borrowing a
//! part of the struct and not the entire thing.

/// Get the current view and document mutably as a tuple.
/// Returns `(&mut View, &mut Document)`
#[macro_export]
macro_rules! current {
    ($editor:expr) => {{
        let dv = &mut $editor.tabs[$editor.active_tab];
        let view = dv.tree.get_mut(dv.tree.focus);
        let doc = &mut dv.doc;
        (view, doc)
    }};
}

#[macro_export]
macro_rules! current_ref {
    ($editor:expr) => {{
        let dv = &$editor.tabs[$editor.active_tab];
        let view = dv.tree.get(dv.tree.focus);
        let doc = &dv.doc;
        (view, doc)
    }};
}

/// Get the document mutably.
/// Returns `&mut Document`
#[macro_export]
macro_rules! doc_mut {
    ($editor:expr) => {{
        &mut $editor.tabs[$editor.active_tab].doc
    }};
}

/// Get the current view mutably.
/// Returns `&mut View`
#[macro_export]
macro_rules! view_mut {
    ($editor:expr, $id:expr) => {{
        $editor.tabs[$editor.active_tab].tree.get_mut($id)
    }};
    ($editor:expr) => {{
        let dv = &mut $editor.tabs[$editor.active_tab];
        dv.tree.get_mut(dv.tree.focus)
    }};
}

/// Get the current view immutably
/// Returns `&View`
#[macro_export]
macro_rules! view {
    ($editor:expr, $id:expr) => {{
        $editor.tabs[$editor.active_tab].tree.get($id)
    }};
    ($editor:expr) => {{
        let dv = &$editor.tabs[$editor.active_tab];
        dv.tree.get(dv.tree.focus)
    }};
}

#[macro_export]
macro_rules! doc {
    ($editor:expr) => {{
        &$editor.tabs[$editor.active_tab].doc
    }};
}
