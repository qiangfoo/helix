//! A popup Component for multi-key sequences (space menu, goto, match, etc.).
//!
//! When a keymap lookup returns `Pending(node)`, the caller pushes a `KeyMenu`
//! with that node. The KeyMenu renders the key hints and handles the next
//! keypress: executing the matched command, descending into a sub-menu, or
//! closing on unknown/escape keys.

use crate::view::graphics::{CursorKind, Rect};
use crate::view::Editor;
use ratatui::buffer::Buffer as Surface;

use crate::commands;
use crate::compositor::{self, Callback, Component, Context, EventResult};
use crate::events::{OnModeSwitch, PostCommand};
use crate::keymap::{KeyTrie, KeyTrieNode};
use crate::layers::EditorLayers;

use super::editor::canonicalize_key;

pub const ID: &str = "key-menu";

/// A transient popup that displays a keymap sub-menu and dispatches the next key.
pub struct KeyMenu {
    node: KeyTrieNode,
    sticky: bool,
}

impl KeyMenu {
    pub fn new(node: KeyTrieNode) -> Self {
        let sticky = node.is_sticky;
        Self { node, sticky }
    }

    fn close_callback() -> Option<Callback> {
        Some(Box::new(|editor: &mut Editor| {
            editor.remove_layer(ID);
            editor.autoinfo = None;
        }))
    }
}

impl Component for KeyMenu {
    fn handle_event(
        &mut self,
        event: &compositor::Event,
        ctx: &mut Context,
    ) -> EventResult {
        let compositor::Event::Key(mut key) = *event else {
            return EventResult::Ignored(None);
        };
        canonicalize_key(&mut key);

        // Escape closes the menu
        if key == crate::key!(Esc) {
            return EventResult::Consumed(Self::close_callback());
        }

        match self.node.get(&key) {
            Some(KeyTrie::MappableCommand(cmd)) => {
                // Execute the command
                let mut cx = commands::Context {
                    editor: ctx.editor,
                    count: None,
                    callback: Vec::new(),
                    jobs: ctx.jobs,
                };

                // Transfer count from active tab
                cx.count = cx.editor.active_doc_view()
                    .and_then(|dv| dv.count);

                let mode_before = cx.editor.mode();
                cmd.execute(&mut cx);
                helix_event::dispatch(PostCommand { command: cmd, cx: &mut cx });

                let mode_after = cx.editor.mode();
                if mode_after != mode_before {
                    helix_event::dispatch(OnModeSwitch {
                        old_mode: mode_before,
                        new_mode: mode_after,
                        cx: &mut cx,
                    });
                }

                // Clear count after execution
                if let Some(dv) = cx.editor.active_doc_view_mut() {
                    dv.count = None;
                }

                let mut callbacks = std::mem::take(&mut cx.callback);

                // Ensure the viewport scrolls to follow the cursor,
                // mirroring what EditorView does after command execution.
                if !cx.editor.should_close() {
                    let config = cx.editor.config();
                    if let Some(dv) = cx.editor.active_doc_view_mut() {
                        let (doc, tree) = dv.doc_and_tree_mut();
                        let view = tree.get_mut(tree.focus);
                        view.ensure_cursor_in_view(doc, config.scrolloff);
                    }
                }

                if !self.sticky {
                    // Pop self
                    callbacks.push(Box::new(|editor: &mut Editor| {
                        editor.remove_layer(ID);
                        editor.autoinfo = None;
                    }));
                }

                let callback: Callback = Box::new(move |editor: &mut Editor| {
                    for cb in callbacks {
                        cb(editor);
                    }
                });
                EventResult::Consumed(Some(callback))
            }
            Some(KeyTrie::Sequence(cmds)) => {
                // Execute all commands in sequence
                let mut cx = commands::Context {
                    editor: ctx.editor,
                    count: None,
                    callback: Vec::new(),
                    jobs: ctx.jobs,
                };
                cx.count = cx.editor.active_doc_view()
                    .and_then(|dv| dv.count);

                let mut last_mode = cx.editor.mode();
                for cmd in cmds {
                    cmd.execute(&mut cx);
                    helix_event::dispatch(PostCommand { command: cmd, cx: &mut cx });
                    let current_mode = cx.editor.mode();
                    if current_mode != last_mode {
                        helix_event::dispatch(OnModeSwitch {
                            old_mode: last_mode,
                            new_mode: current_mode,
                            cx: &mut cx,
                        });
                    }
                    last_mode = current_mode;
                }

                if let Some(dv) = cx.editor.active_doc_view_mut() {
                    dv.count = None;
                }

                let mut callbacks = std::mem::take(&mut cx.callback);

                if !cx.editor.should_close() {
                    let config = cx.editor.config();
                    let (view, doc) = current!(cx.editor);
                    view.ensure_cursor_in_view(doc, config.scrolloff);
                }

                if !self.sticky {
                    callbacks.push(Box::new(|editor: &mut Editor| {
                        editor.remove_layer(ID);
                        editor.autoinfo = None;
                    }));
                }

                let callback: Callback = Box::new(move |editor: &mut Editor| {
                    for cb in callbacks {
                        cb(editor);
                    }
                });
                EventResult::Consumed(Some(callback))
            }
            Some(KeyTrie::Node(subnode)) => {
                // Descend into sub-menu: replace self with deeper KeyMenu
                let submenu = KeyMenu::new(subnode.clone());
                let callback: Callback = Box::new(move |editor: &mut Editor| {
                    editor.replace_or_push_layer(ID, submenu);
                });
                EventResult::Consumed(Some(callback))
            }
            None => {
                // Unknown key — close menu
                EventResult::Consumed(Self::close_callback())
            }
        }
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, ctx: &mut Context) {
        if ctx.editor.config().auto_info {
            let mut info = self.node.infobox();
            info.render(area, surface, ctx);
        }
    }

    fn cursor(&self, _area: Rect, _editor: &Editor) -> (Option<helix_core::Position>, CursorKind) {
        (None, CursorKind::Hidden)
    }

    fn id(&self) -> Option<&'static str> {
        Some(ID)
    }
}
