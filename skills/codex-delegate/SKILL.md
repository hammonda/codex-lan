---
name: codex-delegate
description: Delegate self-contained research, review or implementation subtasks to Codex through the codex-lan MCP bridge using prepared server directories.
---

Use `codex-lan` tools. Claude supplies the task and integrates the result; Rust handles waiting, filtering and native-thread cleanup.

## Prepare

- Paths refer to the server. Call `server_info` only if its workspace root is unknown; request `catalog: true` only to choose a model/effort. Otherwise inherit defaults.
- Prepare a dedicated existing directory inside that root with relevant files and a concise `TASK.md`: objective, necessary project instructions/context, scope, acceptance criteria and deliverables. Codex does not inherit this conversation. The bridge neither uploads files nor checks out repositories; arrange remote files separately if needed. Active directories cannot overlap.
- Pass a short pointer to `TASK.md` instead of repeating its contents. Request file deliverables and a brief final handoff. Coordinate edits to delegated files.

## Delegate

Call `delegate` with `name`, `workspace` and `text`, for example: `Read TASK.md, implement the change and run its checks. Write findings to RESULT.md.`

This single call creates, prompts, waits and deletes the finished native thread. It returns only the final answer, a pending question or a failure. Independent calls can run concurrently. Continue other work if the host backgrounds a call; let its result arrive. **Do not poll, sleep, repeatedly list threads, or drain transcripts.** Human progress, tools and reasoning remain visible in the server inspector.

- `needs_input`: answer the returned request with `thread_reply`; it waits again and cleans up on completion. Resolve genuine missing decisions with the user.
- Cancelled/uncertain call: use `thread_list` to identify the task, then `thread_wait`. Cancellation stops waiting, not work. Use `thread_interrupt` to stop work. Never blindly resubmit.
- Check status and any `cleanup_error`; do not claim success or deletion when unconfirmed. A disconnect cannot resume old work.

Review relevant deliverables and validation, and integrate within the original scope. Final replies default to 4000 bytes; `next_offset` means more is retained. Use `thread_wait` with that offset only if those details are needed. The adapter retains its last 8 completed results until exit; working files persist.

Use manual `thread_create` → `thread_send` → `thread_wait` → `thread_close` only when keeping a multi-turn thread is useful. `thread_read` is opt-in diagnostics: its output consumes Claude context. This skill grants no additional publishing or push authority.
