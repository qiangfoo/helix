---
name: inspect-log
description: Inspect the helix log file for errors and investigate their root cause in the codebase.
user_invocable: true
---

# Inspect Helix Log

Read the helix log file and investigate errors.

## Steps

1. **Read the last 100 lines of the log file** at `~/.cache/helix/helix.log` using Bash (`tail -100`). If the file doesn't exist, tell the user and stop.

2. **Extract error entries.** Look for lines containing `ERROR` or `PANIC` (the log levels used by the `log` crate). Group related consecutive lines (e.g., stack traces or multi-line error messages) into single error entries. Note the timestamp and module path for each.

3. **Present errors to the user.**
   - If there are **no errors**, tell the user the log is clean.
   - If there is **exactly one error**, proceed directly to investigation.
   - If there are **multiple errors**, list them with a short summary (timestamp + first line) and use AskUserQuestion to ask which one to investigate. Number them for easy selection.

4. **Investigate the chosen error.** For the selected error:
   - Identify the module path from the log line (e.g., `helix_term::ui::editor`)
   - Search the codebase for the error message string using Grep
   - Read the relevant source file(s) to understand the code path that produced the error
   - Check if the error is from a `log::error!` call, an `Err` propagation, or a panic
   - Explain what triggered the error, the likely root cause, and suggest a fix if applicable
