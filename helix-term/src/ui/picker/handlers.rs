use std::{
    path::Path,
    sync::{atomic, Arc},
    time::Duration,
};

use helix_event::AsyncHook;
use tokio::time::Instant;

use crate::{job, ui::overlay::Overlay};

use super::{CachedPreview, DynQueryCallback, Picker};

pub(super) struct PreviewHighlightHandler<T: 'static + Send + Sync, D: 'static + Send + Sync> {
    trigger: Option<Arc<Path>>,
    phantom_data: std::marker::PhantomData<(T, D)>,
}

impl<T: 'static + Send + Sync, D: 'static + Send + Sync> Default for PreviewHighlightHandler<T, D> {
    fn default() -> Self {
        Self {
            trigger: None,
            phantom_data: Default::default(),
        }
    }
}

impl<T: 'static + Send + Sync, D: 'static + Send + Sync> AsyncHook
    for PreviewHighlightHandler<T, D>
{
    type Event = Arc<Path>;

    fn handle_event(
        &mut self,
        path: Self::Event,
        timeout: Option<tokio::time::Instant>,
    ) -> Option<tokio::time::Instant> {
        if self
            .trigger
            .as_ref()
            .is_some_and(|trigger| trigger == &path)
        {
            // If the path hasn't changed, don't reset the debounce
            timeout
        } else {
            self.trigger = Some(path);
            Some(Instant::now() + Duration::from_millis(150))
        }
    }

    fn finish_debounce(&mut self) {
        let Some(path) = self.trigger.take() else {
            return;
        };

        job::dispatch_blocking(move |editor| {
            let loader = editor.syn_loader.load();

            // Take layers out to avoid borrow conflicts
            let mut layer_box = std::mem::replace(&mut editor.layer_state, Box::new(()));
            let result = {
                let ls = layer_box
                    .downcast_mut::<crate::layers::LayerState>()
                    .expect("Editor.layer_state must be LayerState");
                let type_name = std::any::type_name::<Overlay<Picker<T, D>>>();
                ls.layers
                    .iter_mut()
                    .find(|c| c.type_name() == type_name)
                    .and_then(|c| c.as_any_mut().downcast_mut::<Overlay<Picker<T, D>>>())
                    .map(|overlay| &mut overlay.content)
                    .and_then(|picker| {
                        let doc = match picker.preview_cache.get_mut(&path) {
                            Some(CachedPreview::Document(doc)) => doc,
                            _ => return None,
                        };
                        if doc.syntax().is_some() {
                            return None;
                        }
                        let language = doc.language_config().map(|config| config.language())?;
                        let text = doc.text().clone();
                        Some((language, text))
                    })
            };
            editor.layer_state = layer_box;

            let Some((language, text)) = result else {
                return;
            };

            tokio::task::spawn_blocking(move || {
                let syntax = match helix_core::Syntax::new(text.slice(..), language, &loader) {
                    Ok(syntax) => syntax,
                    Err(err) => {
                        log::info!("highlighting picker preview failed: {err}");
                        return;
                    }
                };

                job::dispatch_blocking(move |editor| {
                    // Take layers out to avoid borrow conflicts
                    let mut layer_box = std::mem::replace(&mut editor.layer_state, Box::new(()));
                    {
                        let ls = layer_box
                            .downcast_mut::<crate::layers::LayerState>()
                            .expect("Editor.layer_state must be LayerState");
                        let type_name = std::any::type_name::<Overlay<Picker<T, D>>>();
                        if let Some(picker) = ls
                            .layers
                            .iter_mut()
                            .find(|c| c.type_name() == type_name)
                            .and_then(|c| c.as_any_mut().downcast_mut::<Overlay<Picker<T, D>>>())
                            .map(|overlay| &mut overlay.content)
                        {
                            if let Some(CachedPreview::Document(ref mut doc)) =
                                picker.preview_cache.get_mut(&path)
                            {
                                let diagnostics = crate::view::Editor::doc_diagnostics(
                                    &editor.language_servers,
                                    &editor.diagnostics,
                                    doc,
                                );
                                doc.replace_diagnostics(diagnostics, &[], None);
                                doc.syntax = Some(syntax);
                            }
                        } else {
                            log::info!("picker closed before syntax highlighting finished");
                        }
                    }
                    editor.layer_state = layer_box;
                });
            });
        });
    }
}

pub(super) struct DynamicQueryChange {
    pub query: Arc<str>,
    pub is_paste: bool,
}

pub(super) struct DynamicQueryHandler<T: 'static + Send + Sync, D: 'static + Send + Sync> {
    callback: Arc<DynQueryCallback<T, D>>,
    // Duration used as a debounce.
    // Defaults to 100ms if not provided via `Picker::with_dynamic_query`. Callers may want to set
    // this higher if the dynamic query is expensive - for example global search.
    debounce: Duration,
    last_query: Arc<str>,
    query: Option<Arc<str>>,
}

impl<T: 'static + Send + Sync, D: 'static + Send + Sync> DynamicQueryHandler<T, D> {
    pub(super) fn new(callback: DynQueryCallback<T, D>, duration_ms: Option<u64>) -> Self {
        Self {
            callback: Arc::new(callback),
            debounce: Duration::from_millis(duration_ms.unwrap_or(100)),
            last_query: "".into(),
            query: None,
        }
    }
}

impl<T: 'static + Send + Sync, D: 'static + Send + Sync> AsyncHook for DynamicQueryHandler<T, D> {
    type Event = DynamicQueryChange;

    fn handle_event(&mut self, change: Self::Event, _timeout: Option<Instant>) -> Option<Instant> {
        let DynamicQueryChange { query, is_paste } = change;
        if query == self.last_query {
            // If the search query reverts to the last one we requested, no need to
            // make a new request.
            self.query = None;
            None
        } else {
            self.query = Some(query);
            if is_paste {
                self.finish_debounce();
                None
            } else {
                Some(Instant::now() + self.debounce)
            }
        }
    }

    fn finish_debounce(&mut self) {
        let Some(query) = self.query.take() else {
            return;
        };
        self.last_query = query.clone();
        let callback = self.callback.clone();

        job::dispatch_blocking(move |editor| {
            // Take layers out to avoid borrow conflicts between picker and editor
            let mut layer_box = std::mem::replace(&mut editor.layer_state, Box::new(()));
            let result = {
                let ls = layer_box
                    .downcast_mut::<crate::layers::LayerState>()
                    .expect("Editor.layer_state must be LayerState");
                let type_name = std::any::type_name::<Overlay<Picker<T, D>>>();
                ls.layers
                    .iter_mut()
                    .find(|c| c.type_name() == type_name)
                    .and_then(|c| c.as_any_mut().downcast_mut::<Overlay<Picker<T, D>>>())
                    .map(|overlay| &mut overlay.content)
                    .map(|picker| {
                        picker.version.fetch_add(1, atomic::Ordering::Relaxed);
                        picker.matcher.restart(false);
                        let injector = picker.injector();
                        let editor_data = picker.editor_data.clone();
                        (injector, editor_data)
                    })
            };
            editor.layer_state = layer_box;

            let Some((injector, editor_data)) = result else {
                return;
            };
            let get_options = (callback)(&query, editor, editor_data, &injector);
            tokio::spawn(async move {
                if let Err(err) = get_options.await {
                    log::info!("Dynamic request failed: {err}");
                }
                // NOTE: the Drop implementation of Injector will request a redraw when the
                // injector falls out of scope here, clearing the "running" indicator.
            });
        })
    }
}
