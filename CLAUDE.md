# Project Context

This is a **read-only code viewer** built on a fork of Helix. It is not a general-purpose editor.

All buffers are read-only. Write/save commands, force-quit variants, undo/redo, formatting, paste, and other edit-related commands have been removed from the codebase as they serve no purpose here.

# Development Guidelines

- Use `gix` (gitoxide) whenever possible to interact with git instead of running git commands. The `helix-vcs` crate already depends on `gix`.
