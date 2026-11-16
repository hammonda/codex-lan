# Verification

Version **0.5.1**, protocol **5**. Checked on 16 November 2026 with Rust 1.99.0; earlier live checks used Codex CLI 0.167.0-alpha.4.1. The server header displays the compiled application version beside `codex / host`.

## Automated checks

All **88 Linux tests** pass: 30 core/network/usage tests, 35 desktop tests and 23 MCP tests. The **87 Windows tests** also pass under an isolated Wine environment; the extra Linux test covers Unix signal handling. Formatting passes, and strict Clippy checks pass for Linux and the Windows GNU target with all workspace targets and features enabled. Run the commands in the [README](../README.md#development).

| Area | Coverage |
| --- | --- |
| Transport | TLS certificate trust, pairing/version checks, connection ownership, heartbeats, large native events and isolation between clients |
| Lifecycle | Creation, active-turn steering, interruption, close/disconnect/shutdown deletion, failed-deletion retry and cancellation races |
| Workspaces | Automatic codex-lan-threads subfolders, prepared context elsewhere inside the root, overlap/traversal/symlink rejection, per-thread write permissions and retained files |
| Usage | Cumulative snapshots without double counting, cached-token pricing, missing/invalid counts, exact model IDs, account allowance updates and client identity |
| Interface | Output levels, readable/JSON disclosure, streaming, copying, embedded close targets, Enter/Shift+Enter, block cursor, prompt wrapping, live server totals and isolated two-way traffic; chronological steering, completion deduplication, bounded headers, wrapping and pause/resume scrolling |
| MCP | Real stdio subprocesses over TLS, prepared context, concurrent acknowledgements, turn identity, questions, cancellation, lossless pagination, ownership, deletion failures, EOF/forced exit/signals, backend shutdown and pairing-file reconnection without resuming or replaying old work |
| Delegation | One-call creation/wait/cleanup, final-phase filtering, retained results after transcript eviction, intact questions and reply acknowledgement races, concurrent waits/steering/interruption, cancellation during creation, disconnect/backend failure, cleanup errors, UTF-8 result limits and bounded result retention |
| Local setup | Remembered connection-file location, pairing rotation, loopback addresses for wildcard listeners, atomic replacement, owner-only Unix permissions and duplicate-launch activation with lock recovery |

Tests use a deterministic fake Codex executable and local loopback connections. Its log reader tolerates incomplete writes, including split UTF-8 characters, while rejecting malformed complete records. Tests make no OpenAI requests. The optional live examples are separate and consume model usage.

The noisy-delegation fixture emits 60 long commentary messages, native tool/reasoning events and a final answer. The previous default replies path returns about **207 KB over 23 read calls**; `delegate` returns **166 bytes in one call**, with the full native feed still present in the server monitor. This measures model-facing JSON payload bytes, not exact billed tokens or a universal savings rate. No inference is used to filter or summarize results.

## Manual checks and limits

- Linux client and server demos were rendered and visually checked. [Screenshots](images/) contain sample data.
- The server inspector's Replies, Progress, Tools and Everything layouts were captured from rendered X11 demo windows. Fixtures include a multiline command, steering between replies and a completed tool; UI tests also exercise scrolling while new output arrives and returning with Latest.
- Earlier live checks with the installed Codex verified prompt/reply streaming, model/effort selection, active-turn steering, session-local file creation and permanent thread deletion.
- Local setup launched the updated server through its desktop entry with `--start --connection-file`; pairing refreshed with mode 0600 and the remembered path was saved. The adapter verified the `/data/scratch` root, model catalog and reconnection setting; `claude mcp get codex-lan` reported Connected. These setup checks made no model inference calls.
- The Linux launcher passes `desktop-file-validate`, is discoverable by GNOME with its icon, and is pinned in the user's favorites. Its connection-file path matches the installed Claude MCP configuration.
- Repeating a direct launch and two desktop-entry launches kept the same live server PID, one listener and unchanged pairing. The existing window consumed the activation signal, and the MCP metadata check succeeded afterwards.
- The updated Claude skill passes its structural validator and matches the installed personal copy. The release adapter's live metadata check succeeds against `/data/scratch`; MCP discovery advertises `delegate` and `thread_wait`. No real model prompts were sent for this optimization. Native Claude Desktop backgrounding and Code-tab close/archive behavior still need manual verification; tested cleanup follows the MCP process connection.
- Linux release binaries target glibc 2.31. Windows x64 binaries were cross-compiled; native Windows GUI and Codex sandbox behavior still need manual verification on Windows.
- Cleanup is confirmed through `thread/delete`, not archiving. Forced termination or power loss can delay deletion. The journal retries recorded IDs on startup, but native thread creation and local journaling cannot be atomic.
