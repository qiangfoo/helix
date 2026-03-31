use std::time::Duration;

use helix_term::application::Application;
use helix_view::input::parse_macro;
use tokio_stream::wrappers::UnboundedReceiverStream;

#[cfg(windows)]
use crossterm::event::{Event, KeyEvent};
#[cfg(not(windows))]
use termina::event::{Event, KeyEvent};

use super::*;

/// A lightweight helper that sends keys and runs an assertion without
/// calling `current_ref!` — safe to use when `editor.tabs` may be empty
/// (e.g. welcome-tab state).
async fn test_tab_key_sequence(
    app: &mut Application,
    in_keys: Option<&str>,
    test_fn: Option<&dyn Fn(&Application)>,
) -> anyhow::Result<()> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut rx_stream = UnboundedReceiverStream::new(rx);

    if let Some(in_keys) = in_keys {
        for key_event in parse_macro(in_keys)?.into_iter() {
            tx.send(Ok(Event::Key(KeyEvent::from(key_event))))?;
        }
    }

    tokio::time::timeout(
        Duration::from_millis(500),
        app.event_loop_until_idle(&mut rx_stream),
    )
    .await
    .ok();

    if let Some(test) = test_fn {
        test(app);
    }

    Ok(())
}

/// Build an app, run the test body, then cleanly exit.
async fn with_app(
    file: &std::path::Path,
    body: impl AsyncFnOnce(&mut Application) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut app = helpers::AppBuilder::new()
        .with_file(file, None)
        .build()?;

    body(&mut app).await?;

    // Clean up: quit the app
    test_tab_key_sequence(&mut app, Some(":quit<ret>"), None).await?;
    app.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_open_file_creates_tab() -> anyhow::Result<()> {
    let file1 = tempfile::NamedTempFile::new()?;
    let file2 = tempfile::NamedTempFile::new()?;

    with_app(file1.path(), async |app| {
        // Start with 1 tab
        test_tab_key_sequence(
            app,
            None,
            Some(&|app| {
                assert_eq!(1, app.editor.tab_count());
                assert_eq!(0, app.editor.active_tab);
            }),
        )
        .await?;

        // Open a second file
        let open_cmd = format!(":open {}<ret>", file2.path().display());
        test_tab_key_sequence(
            app,
            Some(&open_cmd),
            Some(&|app| {
                assert_eq!(
                    2,
                    app.editor.tab_count(),
                    "should have 2 tabs after opening a second file"
                );
                assert_eq!(1, app.editor.active_tab, "newly opened tab should be active");
            }),
        )
        .await?;

        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_open_same_file_activates_existing_tab() -> anyhow::Result<()> {
    let file = tempfile::NamedTempFile::new()?;

    with_app(file.path(), async |app| {
        // Open the same file again
        let open_cmd = format!(":open {}<ret>", file.path().display());
        test_tab_key_sequence(
            app,
            Some(&open_cmd),
            Some(&|app| {
                assert_eq!(
                    1,
                    app.editor.tab_count(),
                    "opening the same file should not create a duplicate tab"
                );
                assert_eq!(0, app.editor.active_tab);
            }),
        )
        .await?;

        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_close_tab() -> anyhow::Result<()> {
    let file1 = tempfile::NamedTempFile::new()?;
    let file2 = tempfile::NamedTempFile::new()?;

    with_app(file1.path(), async |app| {
        // Open a second file
        let open_cmd = format!(":open {}<ret>", file2.path().display());
        test_tab_key_sequence(
            app,
            Some(&open_cmd),
            Some(&|app| {
                assert_eq!(2, app.editor.tab_count());
            }),
        )
        .await?;

        // Close the active tab
        test_tab_key_sequence(
            app,
            Some(":tab-close<ret>"),
            Some(&|app| {
                assert_eq!(1, app.editor.tab_count(), "should have 1 tab after closing");
            }),
        )
        .await?;

        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_close_last_tab_shows_welcome() -> anyhow::Result<()> {
    let file = tempfile::NamedTempFile::new()?;

    with_app(file.path(), async |app| {
        // Verify we start with one editor tab
        test_tab_key_sequence(
            app,
            None,
            Some(&|app| {
                assert_eq!(1, app.editor.tab_count());
            }),
        )
        .await?;

        // Close the only tab — welcome page should appear
        test_tab_key_sequence(
            app,
            Some(":tab-close<ret>"),
            Some(&|app| {
                assert_eq!(
                    0,
                    app.editor.tab_count(),
                    "after closing last tab, editor should have 0 tabs (welcome page showing)"
                );
            }),
        )
        .await?;

        Ok(())
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_welcome_tab_after_close_all() -> anyhow::Result<()> {
    let file1 = tempfile::NamedTempFile::new()?;
    let file2 = tempfile::NamedTempFile::new()?;

    with_app(file1.path(), async |app| {
        // Open a second file
        let open_cmd = format!(":open {}<ret>", file2.path().display());
        test_tab_key_sequence(
            app,
            Some(&open_cmd),
            Some(&|app| {
                assert_eq!(2, app.editor.tab_count());
            }),
        )
        .await?;

        // Close all tabs
        test_tab_key_sequence(
            app,
            Some(":tab-close-all<ret>"),
            Some(&|app| {
                assert_eq!(
                    0,
                    app.editor.tab_count(),
                    "after tab-close-all, editor should have 0 tabs (welcome page showing)"
                );
            }),
        )
        .await?;

        Ok(())
    })
    .await
}
