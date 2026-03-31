use helix_event::register_hook;
use helix_view::events::DocumentFocusLost;
use helix_view::handlers::Handlers;

use crate::job::{self};
use crate::layers::EditorLayers;
use crate::ui;

pub(super) fn register_hooks(_handlers: &Handlers) {
    register_hook!(move |_event: &mut DocumentFocusLost<'_>| {
        job::dispatch_blocking(move |editor| {
            if editor.find_layer::<ui::Prompt>().is_some() {
                editor.remove_layer_type::<ui::Prompt>();
            }
        });
        Ok(())
    });
}
