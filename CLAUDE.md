# Project Context

This is a **read-only code viewer** built on a fork of Helix. It is not a general-purpose editor.

All buffers are read-only. Write/save commands, force-quit variants, undo/redo, formatting, paste, and other edit-related commands have been removed from the codebase as they serve no purpose here.

# Development Guidelines

- Use `gix` (gitoxide) whenever possible to interact with git instead of running git commands. The `helix-vcs` crate already depends on `gix`.
- The release binary must compile with **zero warnings**. After any code changes, check for and resolve unused imports, dead code, and unused variables before committing. Use `HELIX_DISABLE_AUTO_GRAMMAR_BUILD=1 cargo build --release` to verify.
- At the end of each task, make sure all the test failures are resolved. Run `HELIX_DISABLE_AUTO_GRAMMAR_BUILD=1 cargo test -p helix-term --features integration --test integration` to verify.
- Use the `log` crate (`log::error!`, `log::warn!`, `log::info!`, `log::debug!`) for all logging. The backend is `fern`, configured in `main.rs`. Never use `println!`, `eprintln!`, or manual file writes for diagnostics — they bypass the log file at `~/.cache/helix/helix.log`. Panics are captured by a hook in `main.rs` and logged via `log::error!`.

# Ratatui Architecture Best Practices

## 1. Separate State from UI

The golden rule. Never mix them.

- **`App` struct** = single source of truth. Every piece of mutable data lives here — current screen, user input, loaded data, navigation state, etc.
- **`ui/` functions** = read `&App` (immutable borrow), draw, done. They never mutate state.
- **Mutations** = methods on `App`, called from your event handler.

## 2. Ratatui is Immediate-Mode

Nobody "holds" UI components. Widgets are created fresh every frame, rendered, and thrown away. The only things that persist across frames are the data in `App` — including Ratatui's own stateful helpers like `ListState` and `TableState`, which live in `App`.

## 3. Layered Key Event Handling

Use an `EventResult` enum (`Consumed` / `Propagate`) to chain handlers in priority order. Higher layers (popups, modals) get first dibs; if they don't handle it, the event falls through to lower layers (current screen, then global). For dynamic UIs, maintain a `layer_stack: Vec<Layer>` in `App` and dispatch from the top down.

## 4. Project Structure

```
src/
├── main.rs       # event loop: draw → handle_event → repeat
├── app.rs        # App struct + mutation methods
├── event.rs      # EventResult + input thread
├── tui.rs        # terminal init/restore
└── ui/
    ├── mod.rs    # top-level render(), delegates to screens
    ├── header.rs
    ├── list_screen.rs
    └── popup.rs
```

## The One-Line Mental Model

> **`App` remembers, `ui/` draws, widgets are disposable.**
