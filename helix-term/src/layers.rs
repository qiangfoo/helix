//! Layer management for Editor.
//!
//! Editor stores its layer stack as an opaque `Box<dyn Any>` in `layer_state`.
//! This module provides `LayerState` (the concrete type stored there) and
//! the `EditorLayers` extension trait that adds layer management methods to Editor.

use helix_view::graphics::{CursorKind, Rect};
use helix_view::Editor;

use tui::buffer::Buffer as Surface;

use crate::compositor::{Callback, Component, Context, Event, EventResult};
use crate::ui::picker;

/// The concrete layer state stored inside `Editor.layer_state`.
pub struct LayerState {
    pub layers: Vec<Box<dyn Component>>,
    pub last_picker: Option<Box<dyn Component>>,
    pub full_redraw: bool,
    pub area: Rect,
    /// Keys queued for replay (e.g. from macros). Drained in the main event loop.
    pub pending_keys: Vec<helix_view::input::KeyEvent>,
}

impl LayerState {
    pub fn new(area: Rect) -> Self {
        Self {
            layers: Vec::new(),
            last_picker: None,
            full_redraw: false,
            area,
            pending_keys: Vec::new(),
        }
    }
}

fn layer_state(editor: &Editor) -> &LayerState {
    editor
        .layer_state
        .downcast_ref::<LayerState>()
        .expect("Editor.layer_state must be LayerState")
}

fn layer_state_mut(editor: &mut Editor) -> &mut LayerState {
    editor
        .layer_state
        .downcast_mut::<LayerState>()
        .expect("Editor.layer_state must be LayerState")
}

/// Takes the layer_state box out of Editor, leaving a placeholder.
/// The returned box must be restored via `restore_layers`.
fn take_layers(editor: &mut Editor) -> Box<dyn std::any::Any> {
    std::mem::replace(&mut editor.layer_state, Box::new(()))
}

/// Restores the layer_state box into Editor.
fn restore_layers(editor: &mut Editor, layer_box: Box<dyn std::any::Any>) {
    editor.layer_state = layer_box;
}

/// Extension trait that adds layer management methods to Editor.
pub trait EditorLayers {
    fn init_layers(&mut self, area: Rect);
    fn push_layer(&mut self, layer: Box<dyn Component>);
    fn replace_or_push_layer<T: Component>(&mut self, id: &'static str, layer: T);
    fn pop_layer(&mut self) -> Option<Box<dyn Component>>;
    fn remove_layer(&mut self, id: &'static str) -> Option<Box<dyn Component>>;
    fn remove_layer_type<T: 'static>(&mut self);
    fn has_layer(&self, type_name: &str) -> bool;
    fn find_layer<T: 'static>(&mut self) -> Option<&mut T>;
    fn find_layer_id<T: 'static>(&mut self, id: &'static str) -> Option<&mut T>;
    fn need_full_redraw(&mut self);
    fn layer_area(&self) -> Rect;
    fn resize_layers(&mut self, area: Rect);

    fn queue_macro_keys(&mut self, keys: Vec<helix_view::input::KeyEvent>);
    fn drain_pending_keys(&mut self) -> Vec<helix_view::input::KeyEvent>;

    fn handle_layer_event(&mut self, event: &Event, jobs: &mut crate::job::Jobs) -> bool;
    fn render_layers(&mut self, area: Rect, surface: &mut Surface, jobs: &mut crate::job::Jobs);
    fn layer_cursor(&self, area: Rect) -> (Option<helix_core::Position>, CursorKind);
}

impl EditorLayers for Editor {
    fn init_layers(&mut self, area: Rect) {
        self.layer_state = Box::new(LayerState::new(area));
    }

    fn push_layer(&mut self, mut layer: Box<dyn Component>) {
        let ls = layer_state_mut(self);
        if layer.id() == Some(picker::ID) {
            ls.last_picker = None;
        }
        let size = ls.area;
        layer.required_size((size.width, size.height));
        ls.layers.push(layer);
    }

    fn replace_or_push_layer<T: Component>(&mut self, id: &'static str, layer: T) {
        if let Some(component) = self.find_layer_id(id) {
            *component = layer;
        } else {
            self.push_layer(Box::new(layer))
        }
    }

    fn pop_layer(&mut self) -> Option<Box<dyn Component>> {
        layer_state_mut(self).layers.pop()
    }

    fn remove_layer(&mut self, id: &'static str) -> Option<Box<dyn Component>> {
        let ls = layer_state_mut(self);
        let idx = ls.layers.iter().position(|l| l.id() == Some(id))?;
        Some(ls.layers.remove(idx))
    }

    fn remove_layer_type<T: 'static>(&mut self) {
        let type_name = std::any::type_name::<T>();
        layer_state_mut(self)
            .layers
            .retain(|component| component.type_name() != type_name);
    }

    fn has_layer(&self, type_name: &str) -> bool {
        layer_state(self)
            .layers
            .iter()
            .any(|component| component.type_name() == type_name)
    }

    fn find_layer<T: 'static>(&mut self) -> Option<&mut T> {
        let type_name = std::any::type_name::<T>();
        layer_state_mut(self)
            .layers
            .iter_mut()
            .find(|component| component.type_name() == type_name)
            .and_then(|component| component.as_any_mut().downcast_mut())
    }

    fn find_layer_id<T: 'static>(&mut self, id: &'static str) -> Option<&mut T> {
        layer_state_mut(self)
            .layers
            .iter_mut()
            .find(|component| component.id() == Some(id))
            .and_then(|component| component.as_any_mut().downcast_mut())
    }

    fn need_full_redraw(&mut self) {
        layer_state_mut(self).full_redraw = true;
    }

    fn layer_area(&self) -> Rect {
        layer_state(self).area
    }

    fn resize_layers(&mut self, area: Rect) {
        layer_state_mut(self).area = area;
    }

    fn queue_macro_keys(&mut self, keys: Vec<helix_view::input::KeyEvent>) {
        layer_state_mut(self).pending_keys.extend(keys);
    }

    fn drain_pending_keys(&mut self) -> Vec<helix_view::input::KeyEvent> {
        std::mem::take(&mut layer_state_mut(self).pending_keys)
    }

    fn handle_layer_event(&mut self, event: &Event, jobs: &mut crate::job::Jobs) -> bool {
        // Take layers out of Editor so we can pass &mut Editor to Context
        let mut layer_box = take_layers(self);
        let ls = layer_box
            .downcast_mut::<LayerState>()
            .expect("Editor.layer_state must be LayerState");

        let mut callbacks: Vec<Callback> = Vec::new();
        let mut consumed = false;

        {
            let mut cx = Context {
                editor: self,
                jobs,
                scroll: None,
            };

            for layer in ls.layers.iter_mut().rev() {
                match layer.handle_event(event, &mut cx) {
                    EventResult::Consumed(Some(cb)) => {
                        callbacks.push(cb);
                        consumed = true;
                        break;
                    }
                    EventResult::Consumed(None) => {
                        consumed = true;
                        break;
                    }
                    EventResult::Ignored(Some(cb)) => {
                        callbacks.push(cb);
                    }
                    EventResult::Ignored(None) => {}
                }
            }
        }

        // Restore layers before running callbacks (callbacks may push/pop layers)
        restore_layers(self, layer_box);

        for cb in callbacks {
            cb(self);
        }

        consumed || self.needs_redraw
    }

    fn render_layers(&mut self, area: Rect, surface: &mut Surface, jobs: &mut crate::job::Jobs) {
        let mut layer_box = take_layers(self);
        let ls = layer_box
            .downcast_mut::<LayerState>()
            .expect("Editor.layer_state must be LayerState");

        {
            let mut cx = Context {
                editor: self,
                jobs,
                scroll: None,
            };

            for layer in ls.layers.iter_mut() {
                layer.render(area, surface, &mut cx);
            }
        }

        restore_layers(self, layer_box);
    }

    fn layer_cursor(&self, area: Rect) -> (Option<helix_core::Position>, CursorKind) {
        let ls = layer_state(self);
        for layer in ls.layers.iter().rev() {
            if let (Some(pos), kind) = layer.cursor(area, self) {
                return (Some(pos), kind);
            }
        }
        (None, CursorKind::Hidden)
    }
}
