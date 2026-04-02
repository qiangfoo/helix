pub(crate) mod lsp;
pub(crate) mod syntax;
pub(crate) mod typed;

use futures_util::FutureExt;
use helix_stdx::{
    path::{self, find_paths},
    rope::{self, RopeSliceExt},
};
use helix_vcs::{CommitInfo, Hunk};
pub use lsp::*;
pub use syntax::*;
use ratatui::text::{Span, Line};
pub use typed::*;

use helix_core::{
    char_idx_at_visual_offset,
    chars::char_is_word,
    command_line,
    doc_formatter::TextFormat,
    encoding, find_workspace,
    graphemes,
    indent,
    line_ending::line_end_char_index,
    match_brackets,
    movement::{self, move_vertically_visual, Direction},
    object,
    regex,
    syntax::config::LanguageServerFeature,
    text_annotations::TextAnnotations,
    textobject,
    visual_offset_from_block, Range, RopeReader, RopeSlice,
    Selection, SmallVec,
};
use crate::view::{
    document::{Mode, SCRATCH_BUFFER_NAME},
    editor::Action,
    input::KeyEvent,
    theme::Style,
    tree,
    view::View,
    Document, AppId, Editor,
};

use anyhow::{anyhow, bail, ensure, Context as _};
use arc_swap::access::DynAccess;
use movement::Movement;

use crate::{
    compositor::{self, Component},
    filter_picker_entry,
    job::Callback,
    ui::{self, overlay::overlaid, Picker, PickerColumn, Popup, Prompt, PromptEvent},
};

use crate::job::{self, Jobs};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    fmt,
    future::Future,
    io::Read,
    num::NonZeroUsize,
};

use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use once_cell::sync::Lazy;
use serde::de::{self, Deserialize, Deserializer};
use url::Url;

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{sinks, BinaryDetection, SearcherBuilder};
use ignore::{DirEntry, WalkBuilder, WalkState};

pub struct Context<'a> {
    pub count: Option<NonZeroUsize>,
    pub editor: &'a mut Editor,

    pub callback: Vec<crate::compositor::Callback>,
    pub jobs: &'a mut Jobs,
}

impl Context<'_> {
    /// Push a new component onto the compositor.
    pub fn push_layer(&mut self, component: Box<dyn Component>) {
        self.callback
            .push(Box::new(|editor: &mut Editor| {
                use crate::layers::EditorLayers;
                editor.push_layer(component)
            }));
    }

    /// Call `replace_or_push` on the Compositor
    pub fn replace_or_push_layer<T: Component>(&mut self, id: &'static str, component: T) {
        self.callback
            .push(Box::new(move |editor: &mut Editor| {
                use crate::layers::EditorLayers;
                editor.replace_or_push_layer(id, component);
            }));
    }

    #[inline]
    pub fn callback<T, F>(
        &mut self,
        call: impl Future<Output = helix_lsp::Result<T>> + 'static + Send,
        callback: F,
    ) where
        T: Send + 'static,
        F: FnOnce(&mut Editor, T) + Send + 'static,
    {
        self.jobs.callback(make_job_callback(call, callback));
    }

    /// Returns 1 if no explicit count was provided
    #[inline]
    pub fn count(&self) -> usize {
        self.count.map_or(1, |v| v.get())
    }

    /// Waits on all pending jobs, and then tries to flush all pending write
    /// operations for all documents.
    pub fn block_try_flush_writes(&mut self) -> anyhow::Result<()> {
        compositor::Context {
            editor: self.editor,
            jobs: self.jobs,
            scroll: None,
        }
        .block_try_flush_writes()
    }
}

#[inline]
fn make_job_callback<T, F>(
    call: impl Future<Output = helix_lsp::Result<T>> + 'static + Send,
    callback: F,
) -> std::pin::Pin<Box<impl Future<Output = Result<Callback, anyhow::Error>>>>
where
    T: Send + 'static,
    F: FnOnce(&mut Editor, T) + Send + 'static,
{
    Box::pin(async move {
        let response = call.await?;
        let call: job::Callback = Callback::Editor(Box::new(
            move |editor: &mut Editor| {
                callback(editor, response)
            },
        ));
        Ok(call)
    })
}

use crate::view::{align_view, Align};

/// MappableCommands are commands that can be bound to keys, executable in
/// normal, insert or select mode.
///
/// There are three kinds:
///
/// * Static: commands usually bound to keys and used for editing, movement,
///   etc., for example `move_char_left`.
/// * Typable: commands executable from command mode, prefixed with a `:`,
///   for example `:write!`.
/// * Macro: a sequence of keys to execute, for example `@miw`.
#[derive(Clone)]
pub enum MappableCommand {
    Typable {
        name: String,
        args: String,
        doc: String,
    },
    Static {
        name: &'static str,
        fun: fn(cx: &mut Context),
        doc: &'static str,
    },
    Macro {
        name: String,
        keys: Vec<KeyEvent>,
    },
}

macro_rules! static_commands {
    ( $($name:ident, $doc:literal,)* ) => {
        $(
            #[allow(non_upper_case_globals)]
            pub const $name: Self = Self::Static {
                name: stringify!($name),
                fun: $name,
                doc: $doc
            };
        )*

        pub const STATIC_COMMAND_LIST: &'static [Self] = &[
            $( Self::$name, )*
        ];
    }
}

impl MappableCommand {
    pub fn execute(&self, cx: &mut Context) {
        match &self {
            Self::Typable { name, args, doc: _ } => {
                if let Some(command) = typed::TYPABLE_COMMAND_MAP.get(name.as_str()) {
                    let mut cx = compositor::Context {
                        editor: cx.editor,
                        jobs: cx.jobs,
                        scroll: None,
                    };
                    if let Err(e) =
                        typed::execute_command(&mut cx, command, args, PromptEvent::Validate)
                    {
                        cx.editor.set_error(format!("{}", e));
                    }
                } else {
                    cx.editor.set_error(format!("no such command: '{name}'"));
                }
            }
            Self::Static { fun, .. } => (fun)(cx),
            Self::Macro { keys, .. } => {
                let keys = keys.clone();
                cx.callback.push(Box::new(move |editor: &mut Editor| {
                    use crate::layers::EditorLayers;
                    editor.queue_macro_keys(keys);
                }));
            }
        }
    }

    pub fn name(&self) -> &str {
        match &self {
            Self::Typable { name, .. } => name,
            Self::Static { name, .. } => name,
            Self::Macro { name, .. } => name,
        }
    }

    pub fn doc(&self) -> &str {
        match &self {
            Self::Typable { doc, .. } => doc,
            Self::Static { doc, .. } => doc,
            Self::Macro { name, .. } => name,
        }
    }

    #[rustfmt::skip]
    static_commands!(
        no_op, "Do nothing",
        move_char_left, "Move left",
        move_char_right, "Move right",
        move_line_up, "Move up",
        move_line_down, "Move down",
        move_visual_line_up, "Move up",
        move_visual_line_down, "Move down",
        extend_char_left, "Extend left",
        extend_char_right, "Extend right",
        extend_line_up, "Extend up",
        extend_line_down, "Extend down",
        extend_visual_line_up, "Extend up",
        extend_visual_line_down, "Extend down",
        move_next_word_start, "Move to start of next word",
        move_prev_word_start, "Move to start of previous word",
        move_next_word_end, "Move to end of next word",
        move_prev_word_end, "Move to end of previous word",
        move_next_long_word_start, "Move to start of next long word",
        move_prev_long_word_start, "Move to start of previous long word",
        move_next_long_word_end, "Move to end of next long word",
        move_prev_long_word_end, "Move to end of previous long word",
        move_next_sub_word_start, "Move to start of next sub word",
        move_prev_sub_word_start, "Move to start of previous sub word",
        move_next_sub_word_end, "Move to end of next sub word",
        move_prev_sub_word_end, "Move to end of previous sub word",
        move_parent_node_end, "Move to end of the parent node",
        move_parent_node_start, "Move to beginning of the parent node",
        extend_next_word_start, "Extend to start of next word",
        extend_prev_word_start, "Extend to start of previous word",
        extend_next_word_end, "Extend to end of next word",
        extend_prev_word_end, "Extend to end of previous word",
        extend_next_long_word_start, "Extend to start of next long word",
        extend_prev_long_word_start, "Extend to start of previous long word",
        extend_next_long_word_end, "Extend to end of next long word",
        extend_prev_long_word_end, "Extend to end of prev long word",
        extend_next_sub_word_start, "Extend to start of next sub word",
        extend_prev_sub_word_start, "Extend to start of previous sub word",
        extend_next_sub_word_end, "Extend to end of next sub word",
        extend_prev_sub_word_end, "Extend to end of prev sub word",
        extend_parent_node_end, "Extend to end of the parent node",
        extend_parent_node_start, "Extend to beginning of the parent node",
        page_up, "Move page up",
        page_down, "Move page down",
        half_page_up, "Move half page up",
        half_page_down, "Move half page down",
        page_cursor_up, "Move page and cursor up",
        page_cursor_down, "Move page and cursor down",
        page_cursor_half_up, "Move page and cursor half up",
        page_cursor_half_down, "Move page and cursor half down",
        select_all, "Select whole document",
        search, "Search for regex pattern",
        rsearch, "Reverse search for regex pattern",
        search_next, "Select next search match",
        search_prev, "Select previous search match",
        search_selection, "Use current selection as search pattern",
        search_selection_detect_word_boundaries, "Use current selection as the search pattern, automatically wrapping with `\\b` on word boundaries",
        make_search_word_bounded, "Modify current search to make it word bounded",
        global_search, "Global search in workspace folder",
        extend_line, "Select current line, if already selected, extend to another line based on the anchor",
        extend_line_below, "Select current line, if already selected, extend to next line",
        extend_line_above, "Select current line, if already selected, extend to previous line",
        select_line_above, "Select current line, if already selected, extend or shrink line above based on the anchor",
        select_line_below, "Select current line, if already selected, extend or shrink line below based on the anchor",
        extend_to_line_bounds, "Extend selection to line bounds",
        shrink_to_line_bounds, "Shrink selection to line bounds",
        collapse_selection, "Collapse selection into single cursor",
        flip_selections, "Flip selection cursor and anchor",
        ensure_selections_forward, "Ensure all selections face forward",
        command_mode, "Enter command mode",
        file_picker, "Open file picker",
        file_picker_in_current_buffer_directory, "Open file picker at current buffer's directory",
        file_picker_in_current_directory, "Open file picker at current working directory",
        file_explorer, "Open file explorer in workspace root",
        file_explorer_in_current_buffer_directory, "Open file explorer at current buffer's directory",
        file_explorer_in_current_directory, "Open file explorer at current working directory",
        buffer_picker, "Open tab picker",
        jumplist_picker, "Open jumplist picker",
        symbol_picker, "Open symbol picker",
        syntax_symbol_picker, "Open symbol picker from syntax information",
        lsp_or_syntax_symbol_picker, "Open symbol picker from LSP or syntax information",
        changed_file_picker, "Open changed file picker",
        select_references_to_symbol_under_cursor, "Select symbol references",
        workspace_symbol_picker, "Open workspace symbol picker",
        syntax_workspace_symbol_picker, "Open workspace symbol picker from syntax information",
        lsp_or_syntax_workspace_symbol_picker, "Open workspace symbol picker from LSP or syntax information",
        diagnostics_picker, "Open diagnostic picker",
        workspace_diagnostics_picker, "Open workspace diagnostic picker",
        last_picker, "Open last picker",
        select_mode, "Enter selection extend mode",
        exit_select_mode, "Exit selection mode",
        goto_definition, "Goto definition",
        goto_declaration, "Goto declaration",
        goto_type_definition, "Goto type definition",
        goto_implementation, "Goto implementation",
        goto_file_start, "Goto line number `<n>` else file start",
        goto_file_end, "Goto file end",
        extend_to_file_start, "Extend to line number `<n>` else file start",
        extend_to_file_end, "Extend to file end",
        goto_file, "Goto files/URLs in selections",
        goto_file_hsplit, "Goto files in selections (hsplit)",
        goto_file_vsplit, "Goto files in selections (vsplit)",
        goto_reference, "Goto references",
        goto_window_top, "Goto window top",
        goto_window_center, "Goto window center",
        goto_window_bottom, "Goto window bottom",
        goto_last_accessed_file, "Goto last accessed file",
        goto_last_modified_file, "Goto last modified file",
        goto_line, "Goto line",
        goto_last_line, "Goto last line",
        extend_to_last_line, "Extend to last line",
        goto_first_diag, "Goto first diagnostic",
        goto_last_diag, "Goto last diagnostic",
        goto_next_diag, "Goto next diagnostic",
        goto_prev_diag, "Goto previous diagnostic",
        goto_next_change, "Goto next change",
        goto_prev_change, "Goto previous change",
        goto_first_change, "Goto first change",
        goto_last_change, "Goto last change",
        goto_line_start, "Goto line start",
        goto_line_end, "Goto line end",
        goto_column, "Goto column",
        extend_to_column, "Extend to column",
        goto_next_tab, "Goto next tab",
        goto_previous_tab, "Goto previous tab",
        close_active_tab, "Close the current tab",
        goto_line_end_newline, "Goto newline at line end",
        goto_first_nonwhitespace, "Goto first non-blank in line",
        trim_selections, "Trim whitespace from selections",
        extend_to_line_start, "Extend to line start",
        extend_to_first_nonwhitespace, "Extend to first non-blank in line",
        extend_to_line_end, "Extend to line end",
        extend_to_line_end_newline, "Extend to line end",
        signature_help, "Show signature help",
        yank, "Yank selection",
        yank_to_clipboard, "Yank selections to clipboard",
        yank_to_primary_clipboard, "Yank selections to primary clipboard",
        yank_joined, "Join and yank selections",
        yank_joined_to_clipboard, "Join and yank selections to clipboard",
        yank_main_selection_to_clipboard, "Yank main selection to clipboard",
        yank_location_to_clipboard, "Yank file path and line range to clipboard",
        yank_joined_to_primary_clipboard, "Join and yank selections to primary clipboard",
        yank_main_selection_to_primary_clipboard, "Yank main selection to primary clipboard",
        hover, "Show docs for item under cursor",
        expand_selection, "Expand selection to parent syntax node",
        shrink_selection, "Shrink selection to previously expanded syntax node",
        select_next_sibling, "Select next sibling in the syntax tree",
        select_prev_sibling, "Select previous sibling the in syntax tree",
        jump_forward, "Jump forward on jumplist",
        jump_backward, "Jump backward on jumplist",
        save_selection, "Save current selection to jumplist",
        jump_view_right, "Jump to right split",
        jump_view_left, "Jump to left split",
        jump_view_up, "Jump to split above",
        jump_view_down, "Jump to split below",
        swap_view_right, "Swap with right split",
        swap_view_left, "Swap with left split",
        swap_view_up, "Swap with split above",
        swap_view_down, "Swap with split below",
        transpose_view, "Transpose splits",
        rotate_view, "Goto next window",
        rotate_view_reverse, "Goto previous window",
        hsplit, "Horizontal bottom split",
        hsplit_new, "Horizontal bottom split scratch buffer",
        vsplit, "Vertical right split",
        vsplit_new, "Vertical right split scratch buffer",
        wclose, "Close window",
        wonly, "Close windows except current",
        align_view_middle, "Align view middle",
        align_view_top, "Align view top",
        align_view_center, "Align view center",
        align_view_bottom, "Align view bottom",
        scroll_up, "Scroll view up",
        scroll_down, "Scroll view down",
        match_brackets, "Goto matching bracket",
        select_textobject_around, "Select around object",
        select_textobject_inner, "Select inside object",
        goto_next_function, "Goto next function",
        goto_prev_function, "Goto previous function",
        goto_next_class, "Goto next type definition",
        goto_prev_class, "Goto previous type definition",
        goto_next_parameter, "Goto next parameter",
        goto_prev_parameter, "Goto previous parameter",
        goto_next_comment, "Goto next comment",
        goto_prev_comment, "Goto previous comment",
        goto_next_test, "Goto next test",
        goto_prev_test, "Goto previous test",
        goto_next_xml_element, "Goto next (X)HTML element",
        goto_prev_xml_element, "Goto previous (X)HTML element",
        goto_next_entry, "Goto next pairing",
        goto_prev_entry, "Goto previous pairing",
        goto_next_paragraph, "Goto next paragraph",
        goto_prev_paragraph, "Goto previous paragraph",
        suspend, "Suspend and return to shell",
        command_palette, "Open command palette",
    );
}

impl fmt::Debug for MappableCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MappableCommand::Static { name, .. } => {
                f.debug_tuple("MappableCommand").field(name).finish()
            }
            MappableCommand::Typable { name, args, .. } => f
                .debug_tuple("MappableCommand")
                .field(name)
                .field(args)
                .finish(),
            MappableCommand::Macro { name, keys, .. } => f
                .debug_tuple("MappableCommand")
                .field(name)
                .field(keys)
                .finish(),
        }
    }
}

impl fmt::Display for MappableCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl std::str::FromStr for MappableCommand {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(suffix) = s.strip_prefix(':') {
            let (name, args, _) = command_line::split(suffix);
            ensure!(!name.is_empty(), "Expected typable command name");
            typed::TYPABLE_COMMAND_MAP
                .get(name)
                .map(|cmd| {
                    let doc = if args.is_empty() {
                        cmd.doc.to_string()
                    } else {
                        format!(":{} {:?}", cmd.name, args)
                    };
                    MappableCommand::Typable {
                        name: cmd.name.to_owned(),
                        doc,
                        args: args.to_string(),
                    }
                })
                .ok_or_else(|| anyhow!("No TypableCommand named '{}'", s))
        } else if let Some(suffix) = s.strip_prefix('@') {
            crate::view::input::parse_macro(suffix).map(|keys| Self::Macro {
                name: s.to_string(),
                keys,
            })
        } else {
            MappableCommand::STATIC_COMMAND_LIST
                .iter()
                .find(|cmd| cmd.name() == s)
                .cloned()
                .ok_or_else(|| anyhow!("No command named '{}'", s))
        }
    }
}

impl<'de> Deserialize<'de> for MappableCommand {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(de::Error::custom)
    }
}

impl PartialEq for MappableCommand {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                MappableCommand::Typable {
                    name: first_name,
                    args: first_args,
                    ..
                },
                MappableCommand::Typable {
                    name: second_name,
                    args: second_args,
                    ..
                },
            ) => first_name == second_name && first_args == second_args,
            (
                MappableCommand::Static {
                    name: first_name, ..
                },
                MappableCommand::Static {
                    name: second_name, ..
                },
            ) => first_name == second_name,
            _ => false,
        }
    }
}

fn no_op(_cx: &mut Context) {}

type MoveFn =
    fn(RopeSlice, Range, Direction, usize, Movement, &TextFormat, &mut TextAnnotations) -> Range;

fn move_impl(cx: &mut Context, move_fn: MoveFn, dir: Direction, behaviour: Movement) {
    let count = cx.count();
    let (view, doc) = current!(cx.editor);
    let text = doc.text().slice(..);
    let text_fmt = doc.text_format(view.inner_area(doc).width, None);
    let mut annotations = view.text_annotations(doc, None);

    let selection = doc.selection(view.id).clone().transform(|range| {
        move_fn(
            text,
            range,
            dir,
            count,
            behaviour,
            &text_fmt,
            &mut annotations,
        )
    });
    drop(annotations);
    doc.set_selection(view.id, selection);
}

use helix_core::movement::{move_horizontally, move_vertically};

fn move_char_left(cx: &mut Context) {
    move_impl(cx, move_horizontally, Direction::Backward, Movement::Move)
}

fn move_char_right(cx: &mut Context) {
    move_impl(cx, move_horizontally, Direction::Forward, Movement::Move)
}

fn move_line_up(cx: &mut Context) {
    move_impl(cx, move_vertically, Direction::Backward, Movement::Move)
}

fn move_line_down(cx: &mut Context) {
    move_impl(cx, move_vertically, Direction::Forward, Movement::Move)
}

fn move_visual_line_up(cx: &mut Context) {
    move_impl(
        cx,
        move_vertically_visual,
        Direction::Backward,
        Movement::Move,
    )
}

fn move_visual_line_down(cx: &mut Context) {
    move_impl(
        cx,
        move_vertically_visual,
        Direction::Forward,
        Movement::Move,
    )
}

fn extend_char_left(cx: &mut Context) {
    move_impl(cx, move_horizontally, Direction::Backward, Movement::Extend)
}

fn extend_char_right(cx: &mut Context) {
    move_impl(cx, move_horizontally, Direction::Forward, Movement::Extend)
}

fn extend_line_up(cx: &mut Context) {
    move_impl(cx, move_vertically, Direction::Backward, Movement::Extend)
}

fn extend_line_down(cx: &mut Context) {
    move_impl(cx, move_vertically, Direction::Forward, Movement::Extend)
}

fn extend_visual_line_up(cx: &mut Context) {
    move_impl(
        cx,
        move_vertically_visual,
        Direction::Backward,
        Movement::Extend,
    )
}

fn extend_visual_line_down(cx: &mut Context) {
    move_impl(
        cx,
        move_vertically_visual,
        Direction::Forward,
        Movement::Extend,
    )
}

fn goto_line_end_impl(view: &mut View, doc: &mut Document, movement: Movement) {
    let text = doc.text().slice(..);

    let selection = doc.selection(view.id).clone().transform(|range| {
        let line = range.cursor_line(text);
        let line_start = text.line_to_char(line);

        let pos = graphemes::prev_grapheme_boundary(text, line_end_char_index(&text, line))
            .max(line_start);

        range.put_cursor(text, pos, movement == Movement::Extend)
    });
    doc.set_selection(view.id, selection);
}

fn goto_line_end(cx: &mut Context) {
    let mode = cx.editor.mode();
    let (view, doc) = current!(cx.editor);
    goto_line_end_impl(
        view,
        doc,
        if mode == Mode::Select {
            Movement::Extend
        } else {
            Movement::Move
        },
    )
}

fn extend_to_line_end(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    goto_line_end_impl(view, doc, Movement::Extend)
}

fn goto_line_end_newline_impl(view: &mut View, doc: &mut Document, movement: Movement) {
    let text = doc.text().slice(..);

    let selection = doc.selection(view.id).clone().transform(|range| {
        let line = range.cursor_line(text);
        let pos = line_end_char_index(&text, line);

        range.put_cursor(text, pos, movement == Movement::Extend)
    });
    doc.set_selection(view.id, selection);
}

fn goto_line_end_newline(cx: &mut Context) {
    let mode = cx.editor.mode();
    let (view, doc) = current!(cx.editor);
    goto_line_end_newline_impl(
        view,
        doc,
        if mode == Mode::Select {
            Movement::Extend
        } else {
            Movement::Move
        },
    )
}

fn extend_to_line_end_newline(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    goto_line_end_newline_impl(view, doc, Movement::Extend)
}

fn goto_line_start_impl(view: &mut View, doc: &mut Document, movement: Movement) {
    let text = doc.text().slice(..);

    let selection = doc.selection(view.id).clone().transform(|range| {
        let line = range.cursor_line(text);

        // adjust to start of the line
        let pos = text.line_to_char(line);
        range.put_cursor(text, pos, movement == Movement::Extend)
    });
    doc.set_selection(view.id, selection);
}

fn goto_line_start(cx: &mut Context) {
    let mode = cx.editor.mode();
    let (view, doc) = current!(cx.editor);
    goto_line_start_impl(
        view,
        doc,
        if mode == Mode::Select {
            Movement::Extend
        } else {
            Movement::Move
        },
    )
}

fn goto_next_tab(cx: &mut Context) {
    use crate::ui::EditorApps;
    cx.editor.next_app();
}

fn goto_previous_tab(cx: &mut Context) {
    use crate::ui::EditorApps;
    cx.editor.prev_app();
}

fn close_active_tab(cx: &mut Context) {
    crate::session::save_session(cx.editor);
    cx.callback.push(Box::new(|editor: &mut Editor| {
        use crate::ui::EditorApps;
        editor.close_active_app();
    }));
}

fn extend_to_line_start(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    goto_line_start_impl(view, doc, Movement::Extend)
}

fn goto_first_nonwhitespace(cx: &mut Context) {
    let mode = cx.editor.mode();
    let (view, doc) = current!(cx.editor);

    goto_first_nonwhitespace_impl(
        view,
        doc,
        if mode == Mode::Select {
            Movement::Extend
        } else {
            Movement::Move
        },
    )
}

fn extend_to_first_nonwhitespace(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    goto_first_nonwhitespace_impl(view, doc, Movement::Extend)
}

fn goto_first_nonwhitespace_impl(view: &mut View, doc: &mut Document, movement: Movement) {
    let text = doc.text().slice(..);

    let selection = doc.selection(view.id).clone().transform(|range| {
        let line = range.cursor_line(text);

        if let Some(pos) = text.line(line).first_non_whitespace_char() {
            let pos = pos + text.line_to_char(line);
            range.put_cursor(text, pos, movement == Movement::Extend)
        } else {
            range
        }
    });
    doc.set_selection(view.id, selection);
}

fn trim_selections(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    let text = doc.text().slice(..);

    let ranges: SmallVec<[Range; 1]> = doc
        .selection(view.id)
        .iter()
        .filter_map(|range| {
            if range.is_empty() || range.slice(text).chars().all(|ch| ch.is_whitespace()) {
                return None;
            }
            let mut start = range.from();
            let mut end = range.to();
            start = movement::skip_while(text, start, |x| x.is_whitespace()).unwrap_or(start);
            end = movement::backwards_skip_while(text, end, |x| x.is_whitespace()).unwrap_or(end);
            Some(Range::new(start, end).with_direction(range.direction()))
        })
        .collect();

    if !ranges.is_empty() {
        let primary = doc.selection(view.id).primary();
        let idx = ranges
            .iter()
            .position(|range| range.overlaps(&primary))
            .unwrap_or(ranges.len() - 1);
        doc.set_selection(view.id, Selection::new(ranges, idx));
    } else {
        collapse_selection(cx);
        let (view, doc) = current!(cx.editor);
        let range = doc.selection(view.id).primary();
        doc.set_selection(view.id, Selection::single(range.anchor, range.head));
    };
}

fn goto_window(cx: &mut Context, align: Align) {
    let count = cx.count() - 1;
    let config = cx.editor.config();
    let is_select = cx.editor.mode() == Mode::Select;
    let (view, doc) = current!(cx.editor);
    let view_offset = doc.view_offset(view.id);

    let height = view.inner_height();

    // respect user given count if any
    // - 1 so we have at least one gap in the middle.
    // a height of 6 with padding of 3 on each side will keep shifting the view back and forth
    // as we type
    let scrolloff = config.scrolloff.min(height.saturating_sub(1) / 2);

    let last_visual_line = view.last_visual_line(doc);

    let visual_line = match align {
        Align::Top => view_offset.vertical_offset + scrolloff + count,
        Align::Center => view_offset.vertical_offset + (last_visual_line / 2),
        Align::Bottom => {
            view_offset.vertical_offset + last_visual_line.saturating_sub(scrolloff + count)
        }
    };
    let visual_line = visual_line
        .max(view_offset.vertical_offset + scrolloff)
        .min(view_offset.vertical_offset + last_visual_line.saturating_sub(scrolloff));

    let pos = view
        .pos_at_visual_coords(doc, visual_line as u16, 0, false)
        .expect("visual_line was constrained to the view area");

    let text = doc.text().slice(..);
    let selection = doc
        .selection(view.id)
        .clone()
        .transform(|range| range.put_cursor(text, pos, is_select));
    doc.set_selection(view.id, selection);
}

fn goto_window_top(cx: &mut Context) {
    goto_window(cx, Align::Top)
}

fn goto_window_center(cx: &mut Context) {
    goto_window(cx, Align::Center)
}

fn goto_window_bottom(cx: &mut Context) {
    goto_window(cx, Align::Bottom)
}

fn move_word_impl<F>(cx: &mut Context, move_fn: F)
where
    F: Fn(RopeSlice, Range, usize) -> Range,
{
    let count = cx.count();
    let (view, doc) = current!(cx.editor);
    let text = doc.text().slice(..);

    let selection = doc
        .selection(view.id)
        .clone()
        .transform(|range| move_fn(text, range, count));
    doc.set_selection(view.id, selection);
}

fn move_next_word_start(cx: &mut Context) {
    move_word_impl(cx, movement::move_next_word_start)
}

fn move_prev_word_start(cx: &mut Context) {
    move_word_impl(cx, movement::move_prev_word_start)
}

fn move_prev_word_end(cx: &mut Context) {
    move_word_impl(cx, movement::move_prev_word_end)
}

fn move_next_word_end(cx: &mut Context) {
    move_word_impl(cx, movement::move_next_word_end)
}

fn move_next_long_word_start(cx: &mut Context) {
    move_word_impl(cx, movement::move_next_long_word_start)
}

fn move_prev_long_word_start(cx: &mut Context) {
    move_word_impl(cx, movement::move_prev_long_word_start)
}

fn move_prev_long_word_end(cx: &mut Context) {
    move_word_impl(cx, movement::move_prev_long_word_end)
}

fn move_next_long_word_end(cx: &mut Context) {
    move_word_impl(cx, movement::move_next_long_word_end)
}

fn move_next_sub_word_start(cx: &mut Context) {
    move_word_impl(cx, movement::move_next_sub_word_start)
}

fn move_prev_sub_word_start(cx: &mut Context) {
    move_word_impl(cx, movement::move_prev_sub_word_start)
}

fn move_prev_sub_word_end(cx: &mut Context) {
    move_word_impl(cx, movement::move_prev_sub_word_end)
}

fn move_next_sub_word_end(cx: &mut Context) {
    move_word_impl(cx, movement::move_next_sub_word_end)
}

fn goto_para_impl<F>(cx: &mut Context, move_fn: F)
where
    F: Fn(RopeSlice, Range, usize, Movement) -> Range + 'static,
{
    let count = cx.count();
    let motion = move |editor: &mut Editor| {
        let behavior = if editor.mode() == Mode::Select {
            Movement::Extend
        } else {
            Movement::Move
        };
        let (view, doc) = current!(editor);
        let text = doc.text().slice(..);

        let selection = doc
            .selection(view.id)
            .clone()
            .transform(|range| move_fn(text, range, count, behavior));
        doc.set_selection(view.id, selection);
    };
    cx.editor.apply_motion(motion)
}

fn goto_prev_paragraph(cx: &mut Context) {
    goto_para_impl(cx, movement::move_prev_paragraph)
}

fn goto_next_paragraph(cx: &mut Context) {
    goto_para_impl(cx, movement::move_next_paragraph)
}

fn goto_file_start(cx: &mut Context) {
    goto_file_start_impl(cx, Movement::Move);
}

fn extend_to_file_start(cx: &mut Context) {
    goto_file_start_impl(cx, Movement::Extend);
}

fn goto_file_start_impl(cx: &mut Context, movement: Movement) {
    if cx.count.is_some() {
        goto_line_impl(cx, movement);
    } else {
        let (view, doc) = current!(cx.editor);
        let text = doc.text().slice(..);
        let selection = doc
            .selection(view.id)
            .clone()
            .transform(|range| range.put_cursor(text, 0, movement == Movement::Extend));
        push_jump(view, doc);
        doc.set_selection(view.id, selection);
    }
}

fn goto_file_end(cx: &mut Context) {
    goto_file_end_impl(cx, Movement::Move);
}

fn extend_to_file_end(cx: &mut Context) {
    goto_file_end_impl(cx, Movement::Extend)
}

fn goto_file_end_impl(cx: &mut Context, movement: Movement) {
    let (view, doc) = current!(cx.editor);
    let text = doc.text().slice(..);
    let pos = doc.text().len_chars();
    let selection = doc
        .selection(view.id)
        .clone()
        .transform(|range| range.put_cursor(text, pos, movement == Movement::Extend));
    push_jump(view, doc);
    doc.set_selection(view.id, selection);
}

fn goto_file(cx: &mut Context) {
    goto_file_impl(cx, Action::Replace);
}

fn goto_file_hsplit(cx: &mut Context) {
    goto_file_impl(cx, Action::HorizontalSplit);
}

fn goto_file_vsplit(cx: &mut Context) {
    goto_file_impl(cx, Action::VerticalSplit);
}

/// Returns true when a selection overlaps an LSP document link range.
fn selection_overlaps_document_link(
    selection: &Range,
    link: &crate::view::document::DocumentLink,
) -> bool {
    if selection.is_empty() {
        let pos = selection.from();
        link.start <= pos && pos < link.end
    } else {
        selection.from() < link.end && selection.to() > link.start
    }
}

/// Resolve a document link target, using the LSP resolve request when needed.
fn resolve_document_link_target(
    editor: &Editor,
    link: &crate::view::document::DocumentLink,
) -> Option<Url> {
    if let Some(target) = link.link.target.clone() {
        return Some(target);
    }

    let language_server = editor.language_server_by_id(link.language_server_id)?;
    let supports_resolve = language_server
        .capabilities()
        .document_link_provider
        .as_ref()?
        .resolve_provider
        .unwrap_or(false);

    if !supports_resolve {
        return None;
    }

    let future = language_server.resolve_document_link(link.link.clone())?;
    helix_lsp::block_on(future).ok()?.target
}

/// Goto files/URLs in selection.
///
/// Prefers LSP document links when the cursor/selection overlaps a link range,
/// falling back to the built-in path/URL detection otherwise.
fn goto_file_impl(cx: &mut Context, action: Action) {
    let (view, doc) = current_ref!(cx.editor);
    let text = doc.text().clone();
    let selections = doc.selection(view.id).ranges().to_vec();
    let rel_path = doc
        .relative_path()
        .map(|path| path.parent().unwrap().to_path_buf())
        .unwrap_or_default();
    let text = text.slice(..);

    let mut lsp_targets = Vec::new();
    let mut lsp_targets_seen = HashSet::new();
    let mut fallback_ranges = Vec::new();

    if doc.document_links.is_empty() {
        fallback_ranges.extend_from_slice(&selections);
    } else {
        for selection in &selections {
            let mut matched = false;
            for link in &doc.document_links {
                if !selection_overlaps_document_link(selection, link) {
                    continue;
                }
                matched = true;
                if let Some(target) = resolve_document_link_target(cx.editor, link) {
                    if lsp_targets_seen.insert(target.clone()) {
                        lsp_targets.push(target);
                    }
                }
            }
            if !matched {
                fallback_ranges.push(*selection);
            }
        }
    }

    for target in lsp_targets {
        open_url(cx, target, action);
    }

    if fallback_ranges.is_empty() {
        return;
    }

    let paths: Vec<_> = if fallback_ranges.len() == 1 && fallback_ranges[0].len() == 1 {
        let selection = fallback_ranges[0];
        // Cap the search at roughly 1k bytes around the cursor.
        let lookaround = 1000;
        let pos = text.char_to_byte(selection.cursor(text));
        let search_start = text
            .line_to_byte(text.byte_to_line(pos))
            .max(text.floor_char_boundary(pos.saturating_sub(lookaround)));
        let search_end = text
            .line_to_byte(text.byte_to_line(pos) + 1)
            .min(text.ceil_char_boundary(pos + lookaround));
        let search_range = text.byte_slice(search_start..search_end);
        // we also allow paths that are next to the cursor (can be ambiguous but
        // rarely so in practice) so that gf on quoted/braced path works (not sure about this
        // but apparently that is how gf has worked historically in helix)
        let path = find_paths(search_range, true)
            .take_while(|range| search_start + range.start <= pos + 1)
            .find(|range| pos <= search_start + range.end)
            .map(|range| Cow::from(search_range.byte_slice(range)));
        log::debug!("goto_file auto-detected path: {path:?}");
        let path = path.unwrap_or_else(|| selection.fragment(text));
        vec![path.into_owned()]
    } else {
        // Otherwise use each selection, trimmed.
        fallback_ranges
            .iter()
            .map(|range| range.fragment(text).trim().to_owned())
            .filter(|sel| !sel.is_empty())
            .collect()
    };

    for sel in paths {
        if let Ok(url) = Url::parse(&sel) {
            open_url(cx, url, action);
            continue;
        }

        let path = path::expand(&sel);
        let path = &rel_path.join(path);
        if path.is_dir() {
            let picker = ui::file_picker(cx.editor, path.into());
            cx.push_layer(Box::new(overlaid(picker)));
        } else {
            open_path_as_tab(cx, path.to_path_buf());
        }
    }
}

/// Open a file path by creating a Document and adding it as a new tab via compositor callback.
fn open_path_as_tab(cx: &mut Context, path: PathBuf) {
    match Document::open(
        &path,
        None,
        true,
        cx.editor.config.clone(),
        cx.editor.syn_loader.clone(),
    ) {
        Ok(doc) => {
            let callback: crate::compositor::Callback = Box::new(move |editor: &mut Editor| {
                {
                    use crate::ui::EditorApps;
                    editor.add_editor_app(doc);
                }
            });
            cx.callback.push(callback);
        }
        Err(e) => {
            cx.editor.set_error(format!("Open file failed: {:?}", e));
        }
    }
}

/// Opens the given url. If the URL points to a valid textual file it is open in helix.
//  Otherwise, the file is open using external program.
fn open_url(cx: &mut Context, url: Url, _action: Action) {
    let doc = doc!(cx.editor);
    let rel_path = doc
        .relative_path()
        .map(|path| path.parent().unwrap().to_path_buf())
        .unwrap_or_default();

    if url.scheme() != "file" {
        return cx.jobs.callback(crate::open_external_url_callback(url));
    }

    let content_type = std::fs::File::open(url.path()).and_then(|file| {
        // Read up to 1kb to detect the content type
        let mut read_buffer = Vec::new();
        let n = file.take(1024).read_to_end(&mut read_buffer)?;
        Ok(content_inspector::inspect(&read_buffer[..n]))
    });

    // we attempt to open binary files - files that can't be open in helix - using external
    // program as well, e.g. pdf files or images
    match content_type {
        Ok(content_inspector::ContentType::BINARY) => {
            cx.jobs.callback(crate::open_external_url_callback(url))
        }
        Ok(_) | Err(_) => {
            let path = &rel_path.join(url.path());
            if path.is_dir() {
                let picker = ui::file_picker(cx.editor, path.into());
                cx.push_layer(Box::new(overlaid(picker)));
            } else {
                open_path_as_tab(cx, path.to_path_buf());
            }
        }
    }
}

fn extend_word_impl<F>(cx: &mut Context, extend_fn: F)
where
    F: Fn(RopeSlice, Range, usize) -> Range,
{
    let count = cx.count();
    let (view, doc) = current!(cx.editor);
    let text = doc.text().slice(..);

    let selection = doc.selection(view.id).clone().transform(|range| {
        let word = extend_fn(text, range, count);
        let pos = word.cursor(text);
        range.put_cursor(text, pos, true)
    });
    doc.set_selection(view.id, selection);
}

fn extend_next_word_start(cx: &mut Context) {
    extend_word_impl(cx, movement::move_next_word_start)
}

fn extend_prev_word_start(cx: &mut Context) {
    extend_word_impl(cx, movement::move_prev_word_start)
}

fn extend_next_word_end(cx: &mut Context) {
    extend_word_impl(cx, movement::move_next_word_end)
}

fn extend_prev_word_end(cx: &mut Context) {
    extend_word_impl(cx, movement::move_prev_word_end)
}

fn extend_next_long_word_start(cx: &mut Context) {
    extend_word_impl(cx, movement::move_next_long_word_start)
}

fn extend_prev_long_word_start(cx: &mut Context) {
    extend_word_impl(cx, movement::move_prev_long_word_start)
}

fn extend_prev_long_word_end(cx: &mut Context) {
    extend_word_impl(cx, movement::move_prev_long_word_end)
}

fn extend_next_long_word_end(cx: &mut Context) {
    extend_word_impl(cx, movement::move_next_long_word_end)
}

fn extend_next_sub_word_start(cx: &mut Context) {
    extend_word_impl(cx, movement::move_next_sub_word_start)
}

fn extend_prev_sub_word_start(cx: &mut Context) {
    extend_word_impl(cx, movement::move_prev_sub_word_start)
}

fn extend_prev_sub_word_end(cx: &mut Context) {
    extend_word_impl(cx, movement::move_prev_sub_word_end)
}

fn extend_next_sub_word_end(cx: &mut Context) {
    extend_word_impl(cx, movement::move_next_sub_word_end)
}

pub fn scroll(cx: &mut Context, offset: usize, direction: Direction, sync_cursor: bool) {
    use Direction::*;
    let config = cx.editor.config();
    let mode = cx.editor.mode();
    let (view, doc) = current!(cx.editor);
    let mut view_offset = doc.view_offset(view.id);

    let range = doc.selection(view.id).primary();
    let text = doc.text().slice(..);

    let cursor = range.cursor(text);
    let height = view.inner_height();

    let scrolloff = config.scrolloff.min(height.saturating_sub(1) / 2);
    let offset = match direction {
        Forward => offset as isize,
        Backward => -(offset as isize),
    };

    let doc_text = doc.text().slice(..);
    let viewport = view.inner_area(doc);
    let text_fmt = doc.text_format(viewport.width, None);
    (view_offset.anchor, view_offset.vertical_offset) = char_idx_at_visual_offset(
        doc_text,
        view_offset.anchor,
        view_offset.vertical_offset as isize + offset,
        0,
        &text_fmt,
        // &annotations,
        &view.text_annotations(&*doc, None),
    );
    doc.set_view_offset(view.id, view_offset);

    let doc_text = doc.text().slice(..);
    let mut annotations = view.text_annotations(&*doc, None);

    if sync_cursor {
        let movement = match mode {
            Mode::Select => Movement::Extend,
            _ => Movement::Move,
        };
        // TODO: When inline diagnostics gets merged- 1. move_vertically_visual removes
        // line annotations/diagnostics so the cursor may jump further than the view.
        // 2. If the cursor lands on a complete line of virtual text, the cursor will
        // jump a different distance than the view.
        let selection = doc.selection(view.id).clone().transform(|range| {
            move_vertically_visual(
                doc_text,
                range,
                direction,
                offset.unsigned_abs(),
                movement,
                &text_fmt,
                &mut annotations,
            )
        });
        drop(annotations);
        doc.set_selection(view.id, selection);
        return;
    }

    let view_offset = doc.view_offset(view.id);

    let mut head;
    match direction {
        Forward => {
            let off;
            (head, off) = char_idx_at_visual_offset(
                doc_text,
                view_offset.anchor,
                (view_offset.vertical_offset + scrolloff) as isize,
                0,
                &text_fmt,
                &annotations,
            );
            head += (off != 0) as usize;
            if head <= cursor {
                return;
            }
        }
        Backward => {
            head = char_idx_at_visual_offset(
                doc_text,
                view_offset.anchor,
                (view_offset.vertical_offset + height - scrolloff - 1) as isize,
                0,
                &text_fmt,
                &annotations,
            )
            .0;
            if head >= cursor {
                return;
            }
        }
    }

    let anchor = if mode == Mode::Select {
        range.anchor
    } else {
        head
    };

    // replace primary selection with an empty selection at cursor pos
    let prim_sel = Range::new(anchor, head);
    let mut sel = doc.selection(view.id).clone();
    let idx = sel.primary_index();
    sel = sel.replace(idx, prim_sel);
    drop(annotations);
    doc.set_selection(view.id, sel);
}

fn page_up(cx: &mut Context) {
    let view = view!(cx.editor);
    let offset = view.inner_height();
    scroll(cx, offset, Direction::Backward, false);
}

fn page_down(cx: &mut Context) {
    let view = view!(cx.editor);
    let offset = view.inner_height();
    scroll(cx, offset, Direction::Forward, false);
}

fn half_page_up(cx: &mut Context) {
    let view = view!(cx.editor);
    let offset = view.inner_height() / 2;
    scroll(cx, offset, Direction::Backward, false);
}

fn half_page_down(cx: &mut Context) {
    let view = view!(cx.editor);
    let offset = view.inner_height() / 2;
    scroll(cx, offset, Direction::Forward, false);
}

fn page_cursor_up(cx: &mut Context) {
    let view = view!(cx.editor);
    let offset = view.inner_height();
    scroll(cx, offset, Direction::Backward, true);
}

fn page_cursor_down(cx: &mut Context) {
    let view = view!(cx.editor);
    let offset = view.inner_height();
    scroll(cx, offset, Direction::Forward, true);
}

fn page_cursor_half_up(cx: &mut Context) {
    let view = view!(cx.editor);
    let offset = view.inner_height() / 2;
    scroll(cx, offset, Direction::Backward, true);
}

fn page_cursor_half_down(cx: &mut Context) {
    let view = view!(cx.editor);
    let offset = view.inner_height() / 2;
    scroll(cx, offset, Direction::Forward, true);
}

fn select_all(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);

    let end = doc.text().len_chars();
    doc.set_selection(view.id, Selection::single(0, end))
}

#[allow(clippy::too_many_arguments)]
fn search_impl(
    editor: &mut Editor,
    regex: &rope::Regex,
    movement: Movement,
    direction: Direction,
    scrolloff: usize,
    wrap_around: bool,
    show_warnings: bool,
) {
    let (view, doc) = current!(editor);
    let text = doc.text().slice(..);
    let selection = doc.selection(view.id);

    // Get the right side of the primary block cursor for forward search, or the
    // grapheme before the start of the selection for reverse search.
    let start = match direction {
        Direction::Forward => text.char_to_byte(graphemes::ensure_grapheme_boundary_next(
            text,
            selection.primary().to(),
        )),
        Direction::Backward => text.char_to_byte(graphemes::ensure_grapheme_boundary_prev(
            text,
            selection.primary().from(),
        )),
    };

    // A regex::Match returns byte-positions in the str. In the case where we
    // do a reverse search and wraparound to the end, we don't need to search
    // the text before the current cursor position for matches, but by slicing
    // it out, we need to add it back to the position of the selection.
    let doc = doc!(editor).text().slice(..);

    // use find_at to find the next match after the cursor, loop around the end
    // Careful, `Regex` uses `bytes` as offsets, not character indices!
    let mut mat = match direction {
        Direction::Forward => regex.find(doc.regex_input_at_bytes(start..)),
        Direction::Backward => regex.find_iter(doc.regex_input_at_bytes(..start)).last(),
    };

    if mat.is_none() {
        if wrap_around {
            mat = match direction {
                Direction::Forward => regex.find(doc.regex_input()),
                Direction::Backward => regex.find_iter(doc.regex_input_at_bytes(start..)).last(),
            };
        }
        if show_warnings {
            if wrap_around && mat.is_some() {
                editor.set_status("Wrapped around document");
            } else {
                editor.set_error("No more matches");
            }
        }
    }

    let (view, doc) = current!(editor);
    let text = doc.text().slice(..);
    let selection = doc.selection(view.id);

    if let Some(mat) = mat {
        let start = text.byte_to_char(mat.start());
        let end = text.byte_to_char(mat.end());

        if end == 0 {
            // skip empty matches that don't make sense
            return;
        }

        // Determine range direction based on the primary range
        let primary = selection.primary();
        let range = Range::new(start, end).with_direction(primary.direction());

        let selection = match movement {
            Movement::Extend => selection.clone().push(range),
            Movement::Move => selection.clone().replace(selection.primary_index(), range),
        };

        doc.set_selection(view.id, selection);
        view.ensure_cursor_in_view_center(doc, scrolloff);
    };
}

fn search_completions(cx: &mut Context, reg: Option<char>) -> Vec<String> {
    let mut items = reg
        .and_then(|reg| cx.editor.registers.read(reg, cx.editor))
        .map_or(Vec::new(), |reg| reg.take(200).collect());
    items.sort_unstable();
    items.dedup();
    items.into_iter().map(|value| value.to_string()).collect()
}

fn search(cx: &mut Context) {
    searcher(cx, Direction::Forward)
}

fn rsearch(cx: &mut Context) {
    searcher(cx, Direction::Backward)
}

fn searcher(cx: &mut Context, direction: Direction) {
    let reg = '/';
    let config = cx.editor.config();
    let scrolloff = config.scrolloff;
    let wrap_around = config.search.wrap_around;
    let movement = if cx.editor.mode() == Mode::Select {
        Movement::Extend
    } else {
        Movement::Move
    };

    // TODO: could probably share with select_on_matches?
    let completions = search_completions(cx, Some(reg));

    ui::regex_prompt(
        cx,
        "search:".into(),
        Some(reg),
        move |_editor: &Editor, input: &str| {
            completions
                .iter()
                .filter(|comp| comp.starts_with(input))
                .map(|comp| (0.., comp.clone().into()))
                .collect()
        },
        move |cx, regex, event| {
            if event == PromptEvent::Validate {
                cx.editor.registers.last_search_register = reg;
            } else if event != PromptEvent::Update {
                return;
            }
            search_impl(
                cx.editor,
                &regex,
                movement,
                direction,
                scrolloff,
                wrap_around,
                false,
            );
        },
    );
}

fn search_next_or_prev_impl(cx: &mut Context, movement: Movement, direction: Direction) {
    let count = cx.count();
    let register = cx.editor.registers.last_search_register;
    let config = cx.editor.config();
    let scrolloff = config.scrolloff;
    if let Some(query) = cx.editor.registers.first(register, cx.editor) {
        let search_config = &config.search;
        let case_insensitive = if search_config.smart_case {
            !query.chars().any(char::is_uppercase)
        } else {
            false
        };
        let wrap_around = search_config.wrap_around;
        if let Ok(regex) = rope::RegexBuilder::new()
            .syntax(
                rope::Config::new()
                    .case_insensitive(case_insensitive)
                    .multi_line(true),
            )
            .build(&query)
        {
            for _ in 0..count {
                search_impl(
                    cx.editor,
                    &regex,
                    movement,
                    direction,
                    scrolloff,
                    wrap_around,
                    true,
                );
            }
        } else {
            let error = format!("Invalid regex: {}", query);
            cx.editor.set_error(error);
        }
    }
}

fn search_next(cx: &mut Context) {
    search_next_or_prev_impl(cx, Movement::Move, Direction::Forward);
}

fn search_prev(cx: &mut Context) {
    search_next_or_prev_impl(cx, Movement::Move, Direction::Backward);
}
fn search_selection(cx: &mut Context) {
    search_selection_impl(cx, false)
}

fn search_selection_detect_word_boundaries(cx: &mut Context) {
    search_selection_impl(cx, true)
}

fn search_selection_impl(cx: &mut Context, detect_word_boundaries: bool) {
    fn is_at_word_start(text: RopeSlice, index: usize) -> bool {
        // This can happen when the cursor is at the last character in
        // the document +1 (ge + j), in this case text.char(index) will panic as
        // it will index out of bounds. See https://github.com/helix-editor/helix/issues/12609
        if index == text.len_chars() {
            return false;
        }
        let ch = text.char(index);
        if index == 0 {
            return char_is_word(ch);
        }
        let prev_ch = text.char(index - 1);

        !char_is_word(prev_ch) && char_is_word(ch)
    }

    fn is_at_word_end(text: RopeSlice, index: usize) -> bool {
        if index == 0 || index == text.len_chars() {
            return false;
        }
        let ch = text.char(index);
        let prev_ch = text.char(index - 1);

        char_is_word(prev_ch) && !char_is_word(ch)
    }

    let register = '/';
    let (view, doc) = current!(cx.editor);
    let text = doc.text().slice(..);

    let regex = doc
        .selection(view.id)
        .iter()
        .map(|selection| {
            let add_boundary_prefix =
                detect_word_boundaries && is_at_word_start(text, selection.from());
            let add_boundary_suffix =
                detect_word_boundaries && is_at_word_end(text, selection.to());

            let prefix = if add_boundary_prefix { "\\b" } else { "" };
            let suffix = if add_boundary_suffix { "\\b" } else { "" };

            let word = regex::escape(&selection.fragment(text));
            format!("{}{}{}", prefix, word, suffix)
        })
        .collect::<HashSet<_>>() // Collect into hashset to deduplicate identical regexes
        .into_iter()
        .collect::<Vec<_>>()
        .join("|");

    let msg = format!("register '{}' set to '{}'", register, &regex);
    match cx.editor.registers.push(register, regex) {
        Ok(_) => {
            cx.editor.registers.last_search_register = register;
            cx.editor.set_status(msg)
        }
        Err(err) => cx.editor.set_error(err.to_string()),
    }
}

fn make_search_word_bounded(cx: &mut Context) {
    // Defaults to the active search register instead `/` to be more ergonomic assuming most people
    // would use this command following `search_selection`. This avoids selecting the register
    // twice.
    let register = cx.editor.registers.last_search_register;
    let regex = match cx.editor.registers.first(register, cx.editor) {
        Some(regex) => regex,
        None => return,
    };
    let start_anchored = regex.starts_with("\\b");
    let end_anchored = regex.ends_with("\\b");

    if start_anchored && end_anchored {
        return;
    }

    let mut new_regex = String::with_capacity(
        regex.len() + if start_anchored { 0 } else { 2 } + if end_anchored { 0 } else { 2 },
    );

    if !start_anchored {
        new_regex.push_str("\\b");
    }
    new_regex.push_str(&regex);
    if !end_anchored {
        new_regex.push_str("\\b");
    }

    let msg = format!("register '{}' set to '{}'", register, &new_regex);
    match cx.editor.registers.push(register, new_regex) {
        Ok(_) => {
            cx.editor.registers.last_search_register = register;
            cx.editor.set_status(msg)
        }
        Err(err) => cx.editor.set_error(err.to_string()),
    }
}

fn global_search(cx: &mut Context) {
    #[derive(Debug)]
    struct FileResult {
        path: PathBuf,
        /// 0 indexed line start
        line_start: usize,
        /// 0 indexed line end
        line_end: usize,
    }

    impl FileResult {
        fn new(path: &Path, line_start: usize, line_end: usize) -> Self {
            Self {
                path: path.to_path_buf(),
                line_start,
                line_end,
            }
        }
    }

    struct GlobalSearchConfig {
        smart_case: bool,
        file_picker_config: crate::view::editor::FilePickerConfig,
        directory_style: Style,
        number_style: Style,
        colon_style: Style,
    }

    let config = cx.editor.config();
    let config = GlobalSearchConfig {
        smart_case: config.search.smart_case,
        file_picker_config: config.file_picker.clone(),
        directory_style: cx.editor.theme.get("ui.text.directory"),
        number_style: cx.editor.theme.get("constant.numeric.integer"),
        colon_style: cx.editor.theme.get("punctuation"),
    };

    let columns = [
        PickerColumn::new("path", |item: &FileResult, config: &GlobalSearchConfig| {
            let path = helix_stdx::path::get_relative_path(&item.path);

            let directories = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| format!("{}{}", p.display(), std::path::MAIN_SEPARATOR))
                .unwrap_or_default();

            let filename = item
                .path
                .file_name()
                .expect("global search paths are normalized (can't end in `..`)")
                .to_string_lossy();

            Line::from(vec![
                Span::styled(directories, config.directory_style),
                Span::raw(filename),
                Span::styled(":", config.colon_style),
                Span::styled((item.line_start + 1).to_string(), config.number_style),
            ])
        }),
        PickerColumn::hidden("contents"),
    ];

    let get_files = |query: &str,
                     editor: &mut Editor,
                     config: std::sync::Arc<GlobalSearchConfig>,
                     injector: &ui::picker::Injector<_, _>| {
        if query.is_empty() {
            return async { Ok(()) }.boxed();
        }

        let search_root = helix_stdx::env::current_working_dir();
        if !search_root.exists() {
            return async { Err(anyhow::anyhow!("Current working directory does not exist")) }
                .boxed();
        }

        let documents: Vec<_> = editor.active_doc_view()
            .into_iter()
            .map(|dv| (dv.doc.path().cloned(), dv.doc.text().to_owned()))
            .collect();

        let matcher = match RegexMatcherBuilder::new()
            .case_smart(config.smart_case)
            .multi_line(true)
            .build(query)
        {
            Ok(matcher) => {
                // Clear any "Failed to compile regex" errors out of the statusline.
                editor.clear_status();
                matcher
            }
            Err(err) => {
                log::info!("Failed to compile search pattern in global search: {}", err);
                return async { Err(anyhow::anyhow!("Failed to compile regex")) }.boxed();
            }
        };

        let dedup_symlinks = config.file_picker_config.deduplicate_links;
        let absolute_root = search_root
            .canonicalize()
            .unwrap_or_else(|_| search_root.clone());

        let injector = injector.clone();
        async move {
            let searcher = SearcherBuilder::new()
                .binary_detection(BinaryDetection::quit(b'\x00'))
                .multi_line(true)
                .build();
            WalkBuilder::new(search_root)
                .hidden(config.file_picker_config.hidden)
                .parents(config.file_picker_config.parents)
                .ignore(config.file_picker_config.ignore)
                .follow_links(config.file_picker_config.follow_symlinks)
                .git_ignore(config.file_picker_config.git_ignore)
                .git_global(config.file_picker_config.git_global)
                .git_exclude(config.file_picker_config.git_exclude)
                .max_depth(config.file_picker_config.max_depth)
                .filter_entry(move |entry| {
                    filter_picker_entry(entry, &absolute_root, dedup_symlinks)
                })
                .add_custom_ignore_filename(helix_loader::config_dir().join("ignore"))
                .add_custom_ignore_filename(".helix/ignore")
                .build_parallel()
                .run(|| {
                    let mut searcher = searcher.clone();
                    let matcher = matcher.clone();
                    let injector = injector.clone();
                    let documents = &documents;
                    Box::new(move |entry: Result<DirEntry, ignore::Error>| -> WalkState {
                        let entry = match entry {
                            Ok(entry) => entry,
                            Err(_) => return WalkState::Continue,
                        };

                        if !entry.path().is_file() {
                            return WalkState::Continue;
                        }

                        let mut stop = false;
                        let sink = sinks::UTF8(|line_start, line_content| {
                            let line_start = line_start as usize - 1;
                            let line_end = line_start + line_content.lines().count() - 1;
                            stop = injector
                                .push(FileResult::new(entry.path(), line_start, line_end))
                                .is_err();

                            Ok(!stop)
                        });
                        let doc = documents.iter().find(|&(doc_path, _)| {
                            doc_path
                                .as_ref()
                                .is_some_and(|doc_path| doc_path == entry.path())
                        });

                        let result = if let Some((_, doc)) = doc {
                            // there is already a buffer for this file
                            // search the buffer instead of the file because it's faster
                            // and captures new edits without requiring a save
                            if searcher.multi_line_with_matcher(&matcher) {
                                // in this case a continuous buffer is required
                                // convert the rope to a string
                                let text = doc.to_string();
                                searcher.search_slice(&matcher, text.as_bytes(), sink)
                            } else {
                                searcher.search_reader(
                                    &matcher,
                                    RopeReader::new(doc.slice(..)),
                                    sink,
                                )
                            }
                        } else {
                            searcher.search_path(&matcher, entry.path(), sink)
                        };

                        if let Err(err) = result {
                            log::error!("Global search error: {}, {}", entry.path().display(), err);
                        }
                        if stop {
                            WalkState::Quit
                        } else {
                            WalkState::Continue
                        }
                    })
                });
            Ok(())
        }
        .boxed()
    };

    let reg = '/';
    cx.editor.registers.last_search_register = reg;

    let picker = Picker::new(
        columns,
        1, // contents
        [],
        config,
        move |cx,
              FileResult {
                  path,
                  ..
              },
              _action| {
            match Document::open(
                path,
                None,
                true,
                cx.editor.config.clone(),
                cx.editor.syn_loader.clone(),
            ) {
                Ok(doc) => {
                    let callback = crate::job::Callback::Editor(Box::new(
                        move |editor: &mut Editor| {
                            {
                    use crate::ui::EditorApps;
                    editor.add_editor_app(doc);
                }
                        },
                    ));
                    cx.jobs.callback(async { Ok(callback) });
                }
                Err(e) => {
                    cx.editor
                        .set_error(format!("Failed to open file '{}': {}", path.display(), e));
                }
            }
        },
    )
    .with_preview(
        |_editor,
         FileResult {
             path,
             line_start,
             line_end,
             ..
         }| { Some((path.as_path().into(), Some((*line_start, *line_end)))) },
    )
    .with_history_register(Some(reg))
    .with_dynamic_query(get_files, Some(275));

    cx.push_layer(Box::new(overlaid(picker)));
}

enum Extend {
    Above,
    Below,
}

fn extend_line(cx: &mut Context) {
    let (view, doc) = current_ref!(cx.editor);
    let extend = match doc.selection(view.id).primary().direction() {
        Direction::Forward => Extend::Below,
        Direction::Backward => Extend::Above,
    };
    extend_line_impl(cx, extend);
}

fn extend_line_below(cx: &mut Context) {
    extend_line_impl(cx, Extend::Below);
}

fn extend_line_above(cx: &mut Context) {
    extend_line_impl(cx, Extend::Above);
}
fn extend_line_impl(cx: &mut Context, extend: Extend) {
    let count = cx.count();
    let (view, doc) = current!(cx.editor);

    let text = doc.text();
    let selection = doc.selection(view.id).clone().transform(|range| {
        let (start_line, end_line) = range.line_range(text.slice(..));

        let start = text.line_to_char(start_line);
        let end = text.line_to_char(
            (end_line + 1) // newline of end_line
                .min(text.len_lines()),
        );

        // extend to previous/next line if current line is selected
        let (anchor, head) = if range.from() == start && range.to() == end {
            match extend {
                Extend::Above => (end, text.line_to_char(start_line.saturating_sub(count))),
                Extend::Below => (
                    start,
                    text.line_to_char((end_line + count + 1).min(text.len_lines())),
                ),
            }
        } else {
            match extend {
                Extend::Above => (end, text.line_to_char(start_line.saturating_sub(count - 1))),
                Extend::Below => (
                    start,
                    text.line_to_char((end_line + count).min(text.len_lines())),
                ),
            }
        };

        Range::new(anchor, head)
    });

    doc.set_selection(view.id, selection);
}
fn select_line_below(cx: &mut Context) {
    select_line_impl(cx, Extend::Below);
}
fn select_line_above(cx: &mut Context) {
    select_line_impl(cx, Extend::Above);
}
fn select_line_impl(cx: &mut Context, extend: Extend) {
    let mut count = cx.count();
    let (view, doc) = current!(cx.editor);
    let text = doc.text();
    let saturating_add = |a: usize, b: usize| (a + b).min(text.len_lines());
    let selection = doc.selection(view.id).clone().transform(|range| {
        let (start_line, end_line) = range.line_range(text.slice(..));
        let start = text.line_to_char(start_line);
        let end = text.line_to_char(saturating_add(end_line, 1));
        let direction = range.direction();

        // Extending to line bounds is counted as one step
        if range.from() != start || range.to() != end {
            count = count.saturating_sub(1)
        }
        let (anchor_line, head_line) = match (&extend, direction) {
            (Extend::Above, Direction::Forward) => (start_line, end_line.saturating_sub(count)),
            (Extend::Above, Direction::Backward) => (end_line, start_line.saturating_sub(count)),
            (Extend::Below, Direction::Forward) => (start_line, saturating_add(end_line, count)),
            (Extend::Below, Direction::Backward) => (end_line, saturating_add(start_line, count)),
        };
        let (anchor, head) = match anchor_line.cmp(&head_line) {
            Ordering::Less => (
                text.line_to_char(anchor_line),
                text.line_to_char(saturating_add(head_line, 1)),
            ),
            Ordering::Equal => match extend {
                Extend::Above => (
                    text.line_to_char(saturating_add(anchor_line, 1)),
                    text.line_to_char(head_line),
                ),
                Extend::Below => (
                    text.line_to_char(head_line),
                    text.line_to_char(saturating_add(anchor_line, 1)),
                ),
            },

            Ordering::Greater => (
                text.line_to_char(saturating_add(anchor_line, 1)),
                text.line_to_char(head_line),
            ),
        };
        Range::new(anchor, head)
    });

    doc.set_selection(view.id, selection);
}

fn extend_to_line_bounds(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);

    doc.set_selection(
        view.id,
        doc.selection(view.id).clone().transform(|range| {
            let text = doc.text();

            let (start_line, end_line) = range.line_range(text.slice(..));
            let start = text.line_to_char(start_line);
            let end = text.line_to_char((end_line + 1).min(text.len_lines()));

            Range::new(start, end).with_direction(range.direction())
        }),
    );
}

fn shrink_to_line_bounds(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);

    doc.set_selection(
        view.id,
        doc.selection(view.id).clone().transform(|range| {
            let text = doc.text();

            let (start_line, end_line) = range.line_range(text.slice(..));

            // Do nothing if the selection is within one line to prevent
            // conditional logic for the behavior of this command
            if start_line == end_line {
                return range;
            }

            let mut start = text.line_to_char(start_line);

            // line_to_char gives us the start position of the line, so
            // we need to get the start position of the next line. In
            // the editor, this will correspond to the cursor being on
            // the EOL whitespace character, which is what we want.
            let mut end = text.line_to_char((end_line + 1).min(text.len_lines()));

            if start != range.from() {
                start = text.line_to_char((start_line + 1).min(text.len_lines()));
            }

            if end != range.to() {
                end = text.line_to_char(end_line);
            }

            Range::new(start, end).with_direction(range.direction())
        }),
    );
}

fn collapse_selection(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    let text = doc.text().slice(..);

    let selection = doc.selection(view.id).clone().transform(|range| {
        let pos = range.cursor(text);
        Range::new(pos, pos)
    });
    doc.set_selection(view.id, selection);
}

fn flip_selections(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);

    let selection = doc
        .selection(view.id)
        .clone()
        .transform(|range| range.flip());
    doc.set_selection(view.id, selection);
}

fn ensure_selections_forward(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);

    let selection = doc
        .selection(view.id)
        .clone()
        .transform(|r| r.with_direction(Direction::Forward));

    doc.set_selection(view.id, selection);
}

// inserts at the start of each selection
// inserts at the end of each selection
fn file_picker(cx: &mut Context) {
    let root = find_workspace().0;
    if !root.exists() {
        cx.editor.set_error("Workspace directory does not exist");
        return;
    }
    let picker = ui::file_picker(cx.editor, root);
    cx.push_layer(Box::new(overlaid(picker)));
}

fn file_picker_in_current_buffer_directory(cx: &mut Context) {
    let doc_dir = doc!(cx.editor)
        .path()
        .and_then(|path| path.parent().map(|path| path.to_path_buf()));

    let path = match doc_dir {
        Some(path) => path,
        None => {
            let cwd = helix_stdx::env::current_working_dir();
            if !cwd.exists() {
                cx.editor.set_error(
                    "Current buffer has no parent and current working directory does not exist",
                );
                return;
            }
            cx.editor.set_error(
                "Current buffer has no parent, opening file picker in current working directory",
            );
            cwd
        }
    };

    let picker = ui::file_picker(cx.editor, path);
    cx.push_layer(Box::new(overlaid(picker)));
}

fn file_picker_in_current_directory(cx: &mut Context) {
    let cwd = helix_stdx::env::current_working_dir();
    if !cwd.exists() {
        cx.editor
            .set_error("Current working directory does not exist");
        return;
    }
    let picker = ui::file_picker(cx.editor, cwd);
    cx.push_layer(Box::new(overlaid(picker)));
}

fn file_explorer(cx: &mut Context) {
    let root = find_workspace().0;
    if !root.exists() {
        cx.editor.set_error("Workspace directory does not exist");
        return;
    }

    if let Ok(picker) = ui::file_explorer(root, cx.editor) {
        cx.push_layer(Box::new(overlaid(picker)));
    }
}

fn file_explorer_in_current_buffer_directory(cx: &mut Context) {
    let doc_dir = doc!(cx.editor)
        .path()
        .and_then(|path| path.parent().map(|path| path.to_path_buf()));

    let path = match doc_dir {
        Some(path) => path,
        None => {
            let cwd = helix_stdx::env::current_working_dir();
            if !cwd.exists() {
                cx.editor.set_error(
                    "Current buffer has no parent and current working directory does not exist",
                );
                return;
            }
            cx.editor.set_error(
                "Current buffer has no parent, opening file explorer in current working directory",
            );
            cwd
        }
    };

    if let Ok(picker) = ui::file_explorer(path, cx.editor) {
        cx.push_layer(Box::new(overlaid(picker)));
    }
}

fn file_explorer_in_current_directory(cx: &mut Context) {
    let cwd = helix_stdx::env::current_working_dir();
    if !cwd.exists() {
        cx.editor
            .set_error("Current working directory does not exist");
        return;
    }

    if let Ok(picker) = ui::file_explorer(cwd, cx.editor) {
        cx.push_layer(Box::new(overlaid(picker)));
    }
}

fn buffer_picker(cx: &mut Context) {
    struct TabMeta {
        app_index: usize,
        path: Option<PathBuf>,
        name: String,
        is_active: bool,
    }

    // Gather tab metadata from editor.apps
    cx.callback.push(Box::new(
        |editor: &mut Editor| {
            use crate::layers::EditorLayers;
            let mut items: Vec<TabMeta> = Vec::new();
            let active_index = editor.active_app;
            for i in 0..editor.apps.len() {
                let name = editor.apps[i].name(editor);
                let path = editor.apps[i].icon_path(editor);
                items.push(TabMeta {
                    app_index: i,
                    path,
                    name,
                    is_active: i == active_index,
                });
            }

            let columns = [
                PickerColumn::new("flags", |meta: &TabMeta, _| {
                    if meta.is_active { "*".into() } else { "".into() }
                }),
                PickerColumn::new("name", |meta: &TabMeta, _| {
                    meta.name.clone().into()
                }),
            ];

            let picker = Picker::new(columns, 1, items, (), move |cx, meta, _action| {
                let app_index = meta.app_index;
                cx.jobs.callback(async move {
                    let callback = crate::job::Callback::Editor(Box::new(
                        move |editor| {
                            use crate::ui::EditorApps;
                            editor.switch_app(app_index);
                        },
                    ));
                    Ok(callback)
                });
            })
            .with_preview(|_editor, meta| {
                let path = meta.path.as_deref()?;
                Some((path.into(), None))
            });
            editor.push_layer(Box::new(overlaid(picker)));
        },
    ));
}

fn jumplist_picker(cx: &mut Context) {
    struct JumpMeta {
        id: AppId,
        path: Option<PathBuf>,
        selection: Selection,
        text: String,
        is_current: bool,
    }

    {
        if let Some(dv) = cx.editor.active_doc_view_mut() {
            let (doc, tree) = dv.doc_and_tree_mut();
            for (view, _) in tree.views_mut() {
                view.sync_changes(doc);
            }
        }
    }

    let new_meta = |view: &View, doc_id: AppId, selection: Selection| {
        let doc = cx.editor.active_doc_view()
            .filter(|dv| dv.doc.id() == doc_id)
            .map(|dv| &dv.doc);
        let text = doc.map_or("".into(), |d| {
            selection
                .fragments(d.text().slice(..))
                .map(Cow::into_owned)
                .collect::<Vec<_>>()
                .join(" ")
        });

        JumpMeta {
            id: doc_id,
            path: doc.and_then(|d| d.path().cloned()),
            selection,
            text,
            is_current: view.doc == doc_id,
        }
    };

    let columns = [
        ui::PickerColumn::new("id", |item: &JumpMeta, _| item.id.to_string().into()),
        ui::PickerColumn::new("path", |item: &JumpMeta, _| {
            let path = item
                .path
                .as_deref()
                .map(helix_stdx::path::get_relative_path);
            path.as_deref()
                .and_then(Path::to_str)
                .unwrap_or(SCRATCH_BUFFER_NAME)
                .to_string()
                .into()
        }),
        ui::PickerColumn::new("flags", |item: &JumpMeta, _| {
            let mut flags = Vec::new();
            if item.is_current {
                flags.push("*");
            }

            if flags.is_empty() {
                "".into()
            } else {
                format!(" ({})", flags.join("")).into()
            }
        }),
        ui::PickerColumn::new("contents", |item: &JumpMeta, _| item.text.as_str().into()),
    ];

    let picker = Picker::new(
        columns,
        1, // path
        cx.editor.active_doc_view().unwrap().tree.views().flat_map(|(view, _)| {
            view.jumps
                .iter()
                .rev()
                .map(|(doc_id, selection)| new_meta(view, *doc_id, selection.clone()))
        }),
        (),
        |cx, meta, _action| {
            let config = cx.editor.config();
            let (view, doc) = current!(cx.editor);
            doc.set_selection(view.id, meta.selection.clone());
            view.ensure_cursor_in_view_center(doc, config.scrolloff);
        },
    )
    .with_preview(|editor, meta| {
        let dv = editor.active_doc_view()?;
        if dv.doc.id() == meta.id {
            let line = meta.selection.primary().cursor_line(dv.doc.text().slice(..));
            Some((meta.id.into(), Some((line, line))))
        } else {
            None
        }
    });
    cx.push_layer(Box::new(overlaid(picker)));
}

fn changed_file_picker(cx: &mut Context) {
    enum GitViewItem {
        LocalChanges {
            cwd: PathBuf,
        },
        Commit {
            info: CommitInfo,
            cwd: PathBuf,
        },
    }

    struct GitViewData {
        style_local: Style,
    }

    let cwd = helix_stdx::env::current_working_dir();
    if !cwd.exists() {
        cx.editor
            .set_error("Current working directory does not exist");
        return;
    }

    let style_local = cx.editor.theme.get("diff.delta");
    let diff_providers = cx.editor.diff_providers.clone();

    let columns = [
        PickerColumn::new("hash", |item: &GitViewItem, _data: &GitViewData| {
            match item {
                GitViewItem::LocalChanges { .. } => "".into(),
                GitViewItem::Commit { info, .. } => info.short_hash.as_str().into(),
            }
        }),
        PickerColumn::new("message", |item: &GitViewItem, data: &GitViewData| {
            match item {
                GitViewItem::LocalChanges { .. } => {
                    Span::styled("Local changes", data.style_local).into()
                }
                GitViewItem::Commit { info, .. } => info.message.as_str().into(),
            }
        }),
        PickerColumn::new("author", |item: &GitViewItem, _data: &GitViewData| {
            match item {
                GitViewItem::LocalChanges { .. } => "".into(),
                GitViewItem::Commit { info, .. } => info.author.as_str().into(),
            }
        }),
        PickerColumn::new("date", |item: &GitViewItem, _data: &GitViewData| {
            match item {
                GitViewItem::LocalChanges { .. } => "".into(),
                GitViewItem::Commit { info, .. } => info.date.as_str().into(),
            }
        }),
    ];

    let data = GitViewData {
        style_local,
    };

    let picker = Picker::new(
        columns,
        2, // message column for filtering
        [],
        data,
        |cx, item: &GitViewItem, _action| {
            use crate::ui::diff_view::{DiffKey, DiffView};

            let (diff_key, cwd_clone, files) = match item {
                GitViewItem::LocalChanges { cwd } => (
                    DiffKey::LocalChanges,
                    cwd.clone(),
                    cx.editor
                        .diff_providers
                        .get_local_diff_files(cwd)
                        .unwrap_or_default(),
                ),
                GitViewItem::Commit { info, cwd } => (
                    DiffKey::CommitDiff {
                        hash: info.hash.clone(),
                    },
                    cwd.clone(),
                    cx.editor
                        .diff_providers
                        .get_commit_diff_files(cwd, &info.hash)
                        .unwrap_or_default(),
                ),
            };
            if files.is_empty() {
                cx.editor.set_status("No changes");
                return;
            }
            let callback = crate::job::Callback::Editor(Box::new(
                move |editor: &mut Editor| {
                    use crate::ui::EditorApps;
                    let diff_view = DiffView::new(diff_key, cwd_clone, files, editor);
                    editor.add_diff_app(diff_view);
                },
            ));
            cx.jobs.callback(async { Ok(callback) });
        },
    )
    .with_vi_nav();

    let injector = picker.injector();

    // Stream items in background
    let cwd_clone = cwd.clone();
    tokio::task::spawn_blocking(move || {
        // Push local changes entry if there are any
        if diff_providers.get_local_diff(&cwd_clone).map_or(false, |d| !d.is_empty()) {
            let local_item = GitViewItem::LocalChanges {
                cwd: cwd_clone.clone(),
            };
            if injector.push(local_item).is_err() {
                return;
            }
        }

        // Push commits
        if let Some(commits) = diff_providers.get_commit_log(&cwd_clone, 20) {
            for commit in commits {
                let item = GitViewItem::Commit {
                    info: commit,
                    cwd: cwd_clone.clone(),
                };
                if injector.push(item).is_err() {
                    break;
                }
            }
        }
    });

    cx.push_layer(Box::new(overlaid(picker)));
}

pub fn command_palette(cx: &mut Context) {
    let count = cx.count;

    cx.callback.push(Box::new(
        move |editor: &mut Editor| {
            use crate::layers::EditorLayers;
            // Extract keymap from the active EditorView
            let keymap = {
                let ev_keymaps = editor.apps.get(editor.active_app)
                    .and_then(|app| app.as_any().downcast_ref::<ui::EditorView>())
                    .map(|ev| ev.keymaps.map()[&editor.mode()].reverse_map());
                match ev_keymaps {
                    Some(km) => km,
                    None => return,
                }
            };

            let commands = MappableCommand::STATIC_COMMAND_LIST.iter().cloned().chain(
                typed::TYPABLE_COMMAND_LIST
                    .iter()
                    .map(|cmd| MappableCommand::Typable {
                        name: cmd.name.to_owned(),
                        args: String::new(),
                        doc: cmd.doc.to_owned(),
                    }),
            );

            let columns = [
                ui::PickerColumn::new("name", |item, _| match item {
                    MappableCommand::Typable { name, .. } => format!(":{name}").into(),
                    MappableCommand::Static { name, .. } => (*name).into(),
                    MappableCommand::Macro { .. } => {
                        unreachable!("macros aren't included in the command palette")
                    }
                }),
                ui::PickerColumn::new(
                    "bindings",
                    |item: &MappableCommand, keymap: &crate::keymap::ReverseKeymap| {
                        keymap
                            .get(item.name())
                            .map(|bindings| {
                                bindings.iter().fold(String::new(), |mut acc, bind| {
                                    if !acc.is_empty() {
                                        acc.push(' ');
                                    }
                                    for key in bind {
                                        acc.push_str(&key.key_sequence_format());
                                    }
                                    acc
                                })
                            })
                            .unwrap_or_default()
                            .into()
                    },
                ),
                ui::PickerColumn::new("doc", |item: &MappableCommand, _| item.doc().into()),
            ];

            let picker = Picker::new(columns, 0, commands, keymap, move |cx, command, _action| {
                let mut ctx = Context {
                    count,
                    editor: cx.editor,
                    callback: Vec::new(),

                    jobs: cx.jobs,
                };
                let focus = view!(ctx.editor).id;

                command.execute(&mut ctx);

                if ctx.editor.active_doc_view().is_some_and(|dv| dv.tree.contains(focus)) {
                    let config = ctx.editor.config();
                    let (view, doc) = current!(ctx.editor);
                    view.ensure_cursor_in_view(doc, config.scrolloff);
                }
            });
            editor.push_layer(Box::new(overlaid(picker)));
        },
    ));
}

fn last_picker(cx: &mut Context) {
    // TODO: last picker does not seem to work well with buffer_picker
    cx.callback.push(Box::new(|editor: &mut Editor| {
        let ls = editor.layer_state_mut::<crate::layers::LayerState>();
        if let Some(picker) = ls.last_picker.take() {
            ls.layers.push(picker);
        } else {
            editor.set_error("no last picker")
        }
    }));
}

// Creates an LspCallback that waits for formatting changes to be computed. When they're done,
// it applies them, but only if the doc hasn't changed.
//
// TODO: provide some way to cancel this, probably as part of a more general job cancellation
// scheme
#[derive(PartialEq, Eq)]
pub enum Open {
    Below,
    Above,
}


// o inserts a new line after each line with a selection
// O inserts a new line before each line with a selection
// Store a jump on the jumplist.
fn push_jump(view: &mut View, doc: &Document) {
    let jump = (doc.id(), doc.selection(view.id).clone());
    view.jumps.push(jump);
}

fn goto_line(cx: &mut Context) {
    goto_line_impl(cx, Movement::Move);
}

fn goto_line_impl(cx: &mut Context, movement: Movement) {
    if cx.count.is_some() {
        let (view, doc) = current!(cx.editor);
        push_jump(view, doc);

        goto_line_without_jumplist(cx.editor, cx.count, movement);
    }
}

fn goto_line_without_jumplist(
    editor: &mut Editor,
    count: Option<NonZeroUsize>,
    movement: Movement,
) {
    if let Some(count) = count {
        let (view, doc) = current!(editor);
        let text = doc.text().slice(..);
        let max_line = if text.line(text.len_lines() - 1).len_chars() == 0 {
            // If the last line is blank, don't jump to it.
            text.len_lines().saturating_sub(2)
        } else {
            text.len_lines() - 1
        };
        let line_idx = std::cmp::min(count.get() - 1, max_line);
        let pos = text.line_to_char(line_idx);
        let selection = doc
            .selection(view.id)
            .clone()
            .transform(|range| range.put_cursor(text, pos, movement == Movement::Extend));

        doc.set_selection(view.id, selection);
    }
}

fn goto_last_line(cx: &mut Context) {
    goto_last_line_impl(cx, Movement::Move)
}

fn extend_to_last_line(cx: &mut Context) {
    goto_last_line_impl(cx, Movement::Extend)
}

fn goto_last_line_impl(cx: &mut Context, movement: Movement) {
    let (view, doc) = current!(cx.editor);
    let text = doc.text().slice(..);
    let line_idx = if text.line(text.len_lines() - 1).len_chars() == 0 {
        // If the last line is blank, don't jump to it.
        text.len_lines().saturating_sub(2)
    } else {
        text.len_lines() - 1
    };
    let pos = text.line_to_char(line_idx);
    let selection = doc
        .selection(view.id)
        .clone()
        .transform(|range| range.put_cursor(text, pos, movement == Movement::Extend));

    push_jump(view, doc);
    doc.set_selection(view.id, selection);
}

fn goto_column(cx: &mut Context) {
    goto_column_impl(cx, Movement::Move);
}

fn extend_to_column(cx: &mut Context) {
    goto_column_impl(cx, Movement::Extend);
}

fn goto_column_impl(cx: &mut Context, movement: Movement) {
    let count = cx.count();
    let (view, doc) = current!(cx.editor);
    let text = doc.text().slice(..);
    let selection = doc.selection(view.id).clone().transform(|range| {
        let line = range.cursor_line(text);
        let line_start = text.line_to_char(line);
        let line_end = line_end_char_index(&text, line);
        let pos = graphemes::nth_next_grapheme_boundary(text, line_start, count - 1).min(line_end);
        range.put_cursor(text, pos, movement == Movement::Extend)
    });
    push_jump(view, doc);
    doc.set_selection(view.id, selection);
}

fn goto_last_accessed_file(cx: &mut Context) {
    // In single-doc mode, there is no alternate buffer to switch to.
    cx.editor.set_error("no last accessed buffer");
}

fn goto_last_modified_file(cx: &mut Context) {
    // In single-doc mode, there is no alternate buffer to switch to.
    cx.editor.set_error("no last modified buffer");
}

fn select_mode(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    let text = doc.text().slice(..);

    // Make sure end-of-document selections are also 1-width.
    // (With the exception of being in an empty document, of course.)
    let selection = doc.selection(view.id).clone().transform(|range| {
        if range.is_empty() && range.head == text.len_chars() {
            Range::new(
                graphemes::prev_grapheme_boundary(text, range.anchor),
                range.head,
            )
        } else {
            range
        }
    });
    doc.set_selection(view.id, selection);

    if let Some(dv) = cx.editor.active_doc_view_mut() {
        dv.mode = Mode::Select;
    }
}

fn exit_select_mode(cx: &mut Context) {
    if cx.editor.mode() == Mode::Select {
        if let Some(dv) = cx.editor.active_doc_view_mut() {
            dv.mode = Mode::Normal;
        }
    }
}

fn goto_first_diag(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    let selection = match doc.diagnostics().first() {
        Some(diag) => Selection::single(diag.range.start, diag.range.end),
        None => return,
    };
    push_jump(view, doc);
    doc.set_selection(view.id, selection);
    view.diagnostics_handler
        .immediately_show_diagnostic(doc, view.id);
}

fn goto_last_diag(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    let selection = match doc.diagnostics().last() {
        Some(diag) => Selection::single(diag.range.start, diag.range.end),
        None => return,
    };
    push_jump(view, doc);
    doc.set_selection(view.id, selection);
    view.diagnostics_handler
        .immediately_show_diagnostic(doc, view.id);
}

fn goto_next_diag(cx: &mut Context) {
    let motion = move |editor: &mut Editor| {
        let (view, doc) = current!(editor);

        let cursor_pos = doc
            .selection(view.id)
            .primary()
            .cursor(doc.text().slice(..));

        let diag = doc
            .diagnostics()
            .iter()
            .find(|diag| diag.range.start > cursor_pos);

        let selection = match diag {
            Some(diag) => Selection::single(diag.range.start, diag.range.end),
            None => return,
        };
        push_jump(view, doc);
        doc.set_selection(view.id, selection);
        view.diagnostics_handler
            .immediately_show_diagnostic(doc, view.id);
    };

    cx.editor.apply_motion(motion);
}

fn goto_prev_diag(cx: &mut Context) {
    let motion = move |editor: &mut Editor| {
        let (view, doc) = current!(editor);

        let cursor_pos = doc
            .selection(view.id)
            .primary()
            .cursor(doc.text().slice(..));

        let diag = doc
            .diagnostics()
            .iter()
            .rev()
            .find(|diag| diag.range.start < cursor_pos);

        let selection = match diag {
            // NOTE: the selection is reversed because we're jumping to the
            // previous diagnostic.
            Some(diag) => Selection::single(diag.range.end, diag.range.start),
            None => return,
        };
        push_jump(view, doc);
        doc.set_selection(view.id, selection);
        view.diagnostics_handler
            .immediately_show_diagnostic(doc, view.id);
    };
    cx.editor.apply_motion(motion)
}

fn goto_first_change(cx: &mut Context) {
    goto_first_change_impl(cx, false);
}

fn goto_last_change(cx: &mut Context) {
    goto_first_change_impl(cx, true);
}

fn goto_first_change_impl(cx: &mut Context, reverse: bool) {
    let editor = &mut cx.editor;
    let (view, doc) = current!(editor);
    if let Some(handle) = doc.diff_handle() {
        let hunk = {
            let diff = handle.load();
            let idx = if reverse {
                diff.len().saturating_sub(1)
            } else {
                0
            };
            diff.nth_hunk(idx)
        };
        if hunk != Hunk::NONE {
            let range = hunk_range(hunk, doc.text().slice(..));
            push_jump(view, doc);
            doc.set_selection(view.id, Selection::single(range.anchor, range.head));
        }
    }
}

fn goto_next_change(cx: &mut Context) {
    goto_next_change_impl(cx, Direction::Forward)
}

fn goto_prev_change(cx: &mut Context) {
    goto_next_change_impl(cx, Direction::Backward)
}

fn goto_next_change_impl(cx: &mut Context, direction: Direction) {
    let count = cx.count() as u32 - 1;
    let motion = move |editor: &mut Editor| {
        let is_select = editor.mode() == Mode::Select;
        let (view, doc) = current!(editor);
        let doc_text = doc.text().slice(..);
        let diff_handle = if let Some(diff_handle) = doc.diff_handle() {
            diff_handle
        } else {
            editor.set_status("Diff is not available in current buffer");
            return;
        };

        let selection = doc.selection(view.id).clone().transform(|range| {
            let cursor_line = range.cursor_line(doc_text) as u32;

            let diff = diff_handle.load();
            let hunk_idx = match direction {
                Direction::Forward => diff
                    .next_hunk(cursor_line)
                    .map(|idx| (idx + count).min(diff.len() - 1)),
                Direction::Backward => diff
                    .prev_hunk(cursor_line)
                    .map(|idx| idx.saturating_sub(count)),
            };
            let Some(hunk_idx) = hunk_idx else {
                return range;
            };
            let hunk = diff.nth_hunk(hunk_idx);
            let new_range = hunk_range(hunk, doc_text);
            if is_select {
                let head = if new_range.head < range.anchor {
                    new_range.anchor
                } else {
                    new_range.head
                };

                Range::new(range.anchor, head)
            } else {
                new_range.with_direction(direction)
            }
        });

        push_jump(view, doc);
        doc.set_selection(view.id, selection)
    };
    cx.editor.apply_motion(motion);
}

/// Returns the [Range] for a [Hunk] in the given text.
/// Additions and modifications cover the added and modified ranges.
/// Deletions are represented as the point at the start of the deletion hunk.
fn hunk_range(hunk: Hunk, text: RopeSlice) -> Range {
    let anchor = text.line_to_char(hunk.after.start as usize);
    let head = if hunk.after.is_empty() {
        anchor + 1
    } else {
        text.line_to_char(hunk.after.end as usize)
    };

    Range::new(anchor, head)
}

// Undo / Redo

// Yank / Paste

fn yank(cx: &mut Context) {
    yank_impl(cx.editor, '+');
    exit_select_mode(cx);
}

fn yank_to_clipboard(cx: &mut Context) {
    yank_impl(cx.editor, '+');
    exit_select_mode(cx);
}

fn yank_to_primary_clipboard(cx: &mut Context) {
    yank_impl(cx.editor, '*');
    exit_select_mode(cx);
}

fn yank_impl(editor: &mut Editor, register: char) {
    let (view, doc) = current!(editor);
    let text = doc.text().slice(..);

    let values: Vec<String> = doc
        .selection(view.id)
        .fragments(text)
        .map(Cow::into_owned)
        .collect();
    let selections = values.len();

    match editor.registers.write(register, values) {
        Ok(_) => editor.set_status(format!(
            "yanked {selections} selection{} to clipboard",
            if selections == 1 { "" } else { "s" }
        )),
        Err(err) => editor.set_error(err.to_string()),
    }
}

fn yank_joined_impl(editor: &mut Editor, separator: &str, register: char) {
    let (view, doc) = current!(editor);
    let text = doc.text().slice(..);

    let selection = doc.selection(view.id);
    let selections = selection.len();
    let joined = selection
        .fragments(text)
        .fold(String::new(), |mut acc, fragment| {
            if !acc.is_empty() {
                acc.push_str(separator);
            }
            acc.push_str(&fragment);
            acc
        });

    match editor.registers.write(register, vec![joined]) {
        Ok(_) => editor.set_status(format!(
            "joined and yanked {selections} selection{} to clipboard",
            if selections == 1 { "" } else { "s" }
        )),
        Err(err) => editor.set_error(err.to_string()),
    }
}

fn yank_joined(cx: &mut Context) {
    let separator = doc!(cx.editor).line_ending.as_str();
    yank_joined_impl(cx.editor, separator, '+');
    exit_select_mode(cx);
}

fn yank_joined_to_clipboard(cx: &mut Context) {
    let line_ending = doc!(cx.editor).line_ending;
    yank_joined_impl(cx.editor, line_ending.as_str(), '+');
    exit_select_mode(cx);
}

fn yank_joined_to_primary_clipboard(cx: &mut Context) {
    let line_ending = doc!(cx.editor).line_ending;
    yank_joined_impl(cx.editor, line_ending.as_str(), '*');
    exit_select_mode(cx);
}

pub(crate) fn yank_main_selection_to_register(editor: &mut Editor, register: char) {
    let (view, doc) = current!(editor);
    let text = doc.text().slice(..);

    let selection = doc.selection(view.id).primary().fragment(text).to_string();

    match editor.registers.write(register, vec![selection]) {
        Ok(_) => editor.set_status("yanked primary selection to clipboard"),
        Err(err) => editor.set_error(err.to_string()),
    }
}

fn yank_main_selection_to_clipboard(cx: &mut Context) {
    yank_main_selection_to_register(cx.editor, '+');
    exit_select_mode(cx);
}

fn yank_main_selection_to_primary_clipboard(cx: &mut Context) {
    yank_main_selection_to_register(cx.editor, '*');
    exit_select_mode(cx);
}

fn yank_location_to_clipboard(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    let text = doc.text().slice(..);

    let path = match doc.relative_path() {
        Some(path) => path.to_string_lossy().to_string(),
        None => {
            cx.editor.set_error("buffer has no file path");
            return;
        }
    };

    let primary = doc.selection(view.id).primary();
    let (start_line, end_line) = primary.line_range(text);
    // Convert from 0-based to 1-based line numbers
    let location = if start_line == end_line {
        format!("@{} line {}", path, start_line + 1)
    } else {
        format!("@{} line {}-{}", path, start_line + 1, end_line + 1)
    };

    match cx.editor.registers.write('+', vec![location.clone()]) {
        Ok(_) => cx.editor.set_status(format!("yanked: {location}")),
        Err(err) => cx.editor.set_error(err.to_string()),
    }
    exit_select_mode(cx);
}

// tree sitter node selection

fn expand_selection(cx: &mut Context) {
    let motion = |editor: &mut Editor| {
        let (view, doc) = current!(editor);

        if let Some(syntax) = doc.syntax() {
            let text = doc.text().slice(..);

            let current_selection = doc.selection(view.id);
            let selection = object::expand_selection(syntax, text, current_selection.clone());

            // check if selection is different from the last one
            if *current_selection != selection {
                // save current selection so it can be restored using shrink_selection
                view.object_selections.push(current_selection.clone());

                doc.set_selection(view.id, selection);
            }
        }
    };
    cx.editor.apply_motion(motion);
}

fn shrink_selection(cx: &mut Context) {
    let motion = |editor: &mut Editor| {
        let (view, doc) = current!(editor);
        let current_selection = doc.selection(view.id);
        // try to restore previous selection
        if let Some(prev_selection) = view.object_selections.pop() {
            if current_selection.contains(&prev_selection) {
                doc.set_selection(view.id, prev_selection);
                return;
            } else {
                // clear existing selection as they can't be shrunk to anyway
                view.object_selections.clear();
            }
        }
        // if not previous selection, shrink to first child
        if let Some(syntax) = doc.syntax() {
            let text = doc.text().slice(..);
            let selection = object::shrink_selection(syntax, text, current_selection.clone());
            doc.set_selection(view.id, selection);
        }
    };
    cx.editor.apply_motion(motion);
}

fn select_sibling_impl<F>(cx: &mut Context, sibling_fn: F)
where
    F: Fn(&helix_core::Syntax, RopeSlice, Selection) -> Selection + 'static,
{
    let motion = move |editor: &mut Editor| {
        let (view, doc) = current!(editor);

        if let Some(syntax) = doc.syntax() {
            let text = doc.text().slice(..);
            let current_selection = doc.selection(view.id);
            let selection = sibling_fn(syntax, text, current_selection.clone());
            doc.set_selection(view.id, selection);
        }
    };
    cx.editor.apply_motion(motion);
}

fn select_next_sibling(cx: &mut Context) {
    select_sibling_impl(cx, object::select_next_sibling)
}

fn select_prev_sibling(cx: &mut Context) {
    select_sibling_impl(cx, object::select_prev_sibling)
}

fn move_node_bound_impl(cx: &mut Context, dir: Direction, movement: Movement) {
    let motion = move |editor: &mut Editor| {
        let (view, doc) = current!(editor);

        if let Some(syntax) = doc.syntax() {
            let text = doc.text().slice(..);
            let current_selection = doc.selection(view.id);

            let selection = movement::move_parent_node_end(
                syntax,
                text,
                current_selection.clone(),
                dir,
                movement,
            );

            doc.set_selection(view.id, selection);
        }
    };

    cx.editor.apply_motion(motion);
}

pub fn move_parent_node_end(cx: &mut Context) {
    move_node_bound_impl(cx, Direction::Forward, Movement::Move)
}

pub fn move_parent_node_start(cx: &mut Context) {
    move_node_bound_impl(cx, Direction::Backward, Movement::Move)
}

pub fn extend_parent_node_end(cx: &mut Context) {
    move_node_bound_impl(cx, Direction::Forward, Movement::Extend)
}

pub fn extend_parent_node_start(cx: &mut Context) {
    move_node_bound_impl(cx, Direction::Backward, Movement::Extend)
}

fn match_brackets(cx: &mut Context) {
    let is_select = cx.editor.mode() == Mode::Select;
    let (view, doc) = current!(cx.editor);
    let text = doc.text();
    let text_slice = text.slice(..);

    let selection = doc.selection(view.id).clone().transform(|range| {
        let pos = range.cursor(text_slice);
        if let Some(matched_pos) = doc.syntax().map_or_else(
            || match_brackets::find_matching_bracket_plaintext(text.slice(..), pos),
            |syntax| match_brackets::find_matching_bracket_fuzzy(syntax, text.slice(..), pos),
        ) {
            range.put_cursor(text_slice, matched_pos, is_select)
        } else {
            range
        }
    });

    doc.set_selection(view.id, selection);
}

//

fn jump_forward(cx: &mut Context) {
    let focus = cx.editor.active_doc_view().unwrap().tree.focus;
    cx.editor.jump_forward(focus, cx.count());
}

fn jump_backward(cx: &mut Context) {
    let focus = cx.editor.active_doc_view().unwrap().tree.focus;
    cx.editor.jump_backward(focus, cx.count());
}

fn save_selection(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    push_jump(view, doc);
    cx.editor.set_status("Selection saved to jumplist");
}

fn rotate_view(cx: &mut Context) {
    cx.editor.focus_next()
}

fn rotate_view_reverse(cx: &mut Context) {
    cx.editor.focus_prev()
}

fn jump_view_right(cx: &mut Context) {
    cx.editor.focus_direction(tree::Direction::Right)
}

fn jump_view_left(cx: &mut Context) {
    cx.editor.focus_direction(tree::Direction::Left)
}

fn jump_view_up(cx: &mut Context) {
    cx.editor.focus_direction(tree::Direction::Up)
}

fn jump_view_down(cx: &mut Context) {
    cx.editor.focus_direction(tree::Direction::Down)
}

fn swap_view_right(cx: &mut Context) {
    cx.editor.swap_split_in_direction(tree::Direction::Right)
}

fn swap_view_left(cx: &mut Context) {
    cx.editor.swap_split_in_direction(tree::Direction::Left)
}

fn swap_view_up(cx: &mut Context) {
    cx.editor.swap_split_in_direction(tree::Direction::Up)
}

fn swap_view_down(cx: &mut Context) {
    cx.editor.swap_split_in_direction(tree::Direction::Down)
}

fn transpose_view(cx: &mut Context) {
    cx.editor.transpose_view()
}

/// Open a new split in the given direction specified by the action.
///
/// Maintain the current view (both the cursor's position and view in document).
fn split(editor: &mut Editor, action: Action) {
    let (view, doc) = current!(editor);
    let id = doc.id();
    let selection = doc.selection(view.id).clone();
    let offset = doc.view_offset(view.id);

    // Create a new split view for the same document
    let new_view = View::new(id, editor.config().gutters.clone());
    let layout = match action {
        Action::HorizontalSplit => crate::view::tree::Layout::Horizontal,
        _ => crate::view::tree::Layout::Vertical,
    };
    editor.active_doc_view_mut().unwrap().tree.split(new_view, layout);

    // match the selection in the new view
    let (view, doc) = current!(editor);
    doc.set_selection(view.id, selection);
    doc.set_view_offset(view.id, offset);
}

fn hsplit(cx: &mut Context) {
    split(cx.editor, Action::HorizontalSplit);
}

fn hsplit_new(cx: &mut Context) {
    // In read-only viewer, just split with the same document
    split(cx.editor, Action::HorizontalSplit);
}

fn vsplit(cx: &mut Context) {
    split(cx.editor, Action::VerticalSplit);
}

fn vsplit_new(cx: &mut Context) {
    // In read-only viewer, just split with the same document
    split(cx.editor, Action::VerticalSplit);
}

fn wclose(cx: &mut Context) {
    if cx.editor.active_doc_view().unwrap().tree.views().count() == 1 {
        if let Err(err) = typed::buffers_remaining_impl(cx.editor) {
            cx.editor.set_error(err.to_string());
            return;
        }
    }
    let view_id = view!(cx.editor).id;
    // close current split
    if cx.editor.close(view_id) {
        // Tree is empty — close the tab via TabManager callback
        cx.callback.push(Box::new(|editor: &mut Editor| {
            use crate::ui::EditorApps;
            editor.close_active_app();
        }));
    }
}

fn wonly(cx: &mut Context) {
    let views = cx
        .editor
        .active_doc_view()
        .unwrap()
        .tree
        .views()
        .map(|(v, focus)| (v.id, focus))
        .collect::<Vec<_>>();
    for (view_id, focus) in views {
        if !focus {
            cx.editor.close(view_id);
        }
    }
}

fn align_view_top(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    align_view(doc, view, Align::Top);
}

fn align_view_center(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    align_view(doc, view, Align::Center);
}

fn align_view_bottom(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    align_view(doc, view, Align::Bottom);
}

fn align_view_middle(cx: &mut Context) {
    let (view, doc) = current!(cx.editor);
    let inner_width = view.inner_width(doc);
    let text_fmt = doc.text_format(inner_width, None);
    // there is no horizontal position when softwrap is enabled
    if text_fmt.soft_wrap {
        return;
    }
    let doc_text = doc.text().slice(..);
    let pos = doc.selection(view.id).primary().cursor(doc_text);
    let pos = visual_offset_from_block(
        doc_text,
        doc.view_offset(view.id).anchor,
        pos,
        &text_fmt,
        &view.text_annotations(doc, None),
    )
    .0;

    let mut offset = doc.view_offset(view.id);
    offset.horizontal_offset = pos
        .col
        .saturating_sub((view.inner_area(doc).width as usize) / 2);
    doc.set_view_offset(view.id, offset);
}

fn scroll_up(cx: &mut Context) {
    scroll(cx, cx.count(), Direction::Backward, false);
}

fn scroll_down(cx: &mut Context) {
    scroll(cx, cx.count(), Direction::Forward, false);
}

fn goto_ts_object_impl(cx: &mut Context, object: &'static str, direction: Direction) {
    let count = cx.count();
    let motion = move |editor: &mut Editor| {
        let is_select = editor.mode() == Mode::Select;
        let (view, doc) = current!(editor);
        let loader = editor.syn_loader.load();
        if let Some(syntax) = doc.syntax() {
            let text = doc.text().slice(..);
            let root = syntax.tree().root_node();

            let selection = doc.selection(view.id).clone().transform(|range| {
                let new_range = movement::goto_treesitter_object(
                    text, range, object, direction, &root, syntax, &loader, count,
                );

                if is_select {
                    let head = if new_range.head < range.anchor {
                        new_range.anchor
                    } else {
                        new_range.head
                    };

                    Range::new(range.anchor, head)
                } else {
                    new_range.with_direction(direction)
                }
            });

            push_jump(view, doc);
            doc.set_selection(view.id, selection);
        } else {
            editor.set_status("Syntax-tree is not available in current buffer");
        }
    };
    cx.editor.apply_motion(motion);
}

fn goto_next_function(cx: &mut Context) {
    goto_ts_object_impl(cx, "function", Direction::Forward)
}

fn goto_prev_function(cx: &mut Context) {
    goto_ts_object_impl(cx, "function", Direction::Backward)
}

fn goto_next_class(cx: &mut Context) {
    goto_ts_object_impl(cx, "class", Direction::Forward)
}

fn goto_prev_class(cx: &mut Context) {
    goto_ts_object_impl(cx, "class", Direction::Backward)
}

fn goto_next_parameter(cx: &mut Context) {
    goto_ts_object_impl(cx, "parameter", Direction::Forward)
}

fn goto_prev_parameter(cx: &mut Context) {
    goto_ts_object_impl(cx, "parameter", Direction::Backward)
}

fn goto_next_comment(cx: &mut Context) {
    goto_ts_object_impl(cx, "comment", Direction::Forward)
}

fn goto_prev_comment(cx: &mut Context) {
    goto_ts_object_impl(cx, "comment", Direction::Backward)
}

fn goto_next_test(cx: &mut Context) {
    goto_ts_object_impl(cx, "test", Direction::Forward)
}

fn goto_prev_test(cx: &mut Context) {
    goto_ts_object_impl(cx, "test", Direction::Backward)
}

fn goto_next_xml_element(cx: &mut Context) {
    goto_ts_object_impl(cx, "xml-element", Direction::Forward)
}

fn goto_prev_xml_element(cx: &mut Context) {
    goto_ts_object_impl(cx, "xml-element", Direction::Backward)
}

fn goto_next_entry(cx: &mut Context) {
    goto_ts_object_impl(cx, "entry", Direction::Forward)
}

fn goto_prev_entry(cx: &mut Context) {
    goto_ts_object_impl(cx, "entry", Direction::Backward)
}

fn select_textobject_around(cx: &mut Context) {
    select_textobject(cx, textobject::TextObject::Around);
}

fn select_textobject_inner(cx: &mut Context) {
    select_textobject(cx, textobject::TextObject::Inside);
}

fn select_textobject(cx: &mut Context, objtype: textobject::TextObject) {
    let count = cx.count();
    let menu = crate::ui::textobject_menu::TextObjectMenu::new(objtype, count);
    cx.push_layer(Box::new(menu));
}

fn suspend(_cx: &mut Context) {
    #[cfg(not(windows))]
    {
        // SAFETY: These are calls to standard POSIX functions.
        // Unsafe is necessary since we are calling outside of Rust.
        let is_session_leader = unsafe { libc::getpid() == libc::getsid(0) };

        // If helix is the session leader, there is nothing to suspend to, so skip
        if is_session_leader {
            return;
        }
        _cx.block_try_flush_writes().ok();
        signal_hook::low_level::raise(signal_hook::consts::signal::SIGTSTP).unwrap();
    }
}

fn lsp_or_syntax_symbol_picker(cx: &mut Context) {
    let doc = doc!(cx.editor);

    if doc
        .language_servers_with_feature(LanguageServerFeature::DocumentSymbols)
        .next()
        .is_some()
    {
        lsp::symbol_picker(cx);
    } else if doc.syntax().is_some() {
        syntax_symbol_picker(cx);
    } else {
        cx.editor
            .set_error("No language server supporting document symbols or syntax info available");
    }
}

fn lsp_or_syntax_workspace_symbol_picker(cx: &mut Context) {
    let doc = doc!(cx.editor);

    if doc
        .language_servers_with_feature(LanguageServerFeature::WorkspaceSymbols)
        .next()
        .is_some()
    {
        lsp::workspace_symbol_picker(cx);
    } else {
        syntax_workspace_symbol_picker(cx);
    }
}
