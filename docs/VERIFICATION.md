# Verification

Version **0.4.4**, protocol **4**. Checked on 15 November 2026 with Rust 1.99.0 and Codex CLI 0.167.0-alpha.4.1.

## Automated checks

All **55 tests** pass: 22 core/network tests and 33 desktop tests. Formatting and strict Clippy checks pass with all workspace targets and features enabled. Run the commands in the [README](../README.md#development).

| Area | Coverage |
| --- | --- |
| Transport | TLS certificate trust, pairing/version checks, connection ownership, heartbeats, large native events and isolation between clients |
| Lifecycle | Creation, active-turn steering, interruption, close/disconnect/shutdown deletion, failed-deletion retry and cancellation races |
| Workspaces | Unique session folders, inside-root overrides, traversal/symlink rejection, permissions and retained files |
| Usage | Cumulative snapshots without double counting, cached-token pricing, missing/invalid counts, exact model IDs, account allowance updates and client identity |
| Interface | Output levels, readable/JSON disclosure, streaming, copying, embedded close targets, Enter/Shift+Enter, block cursor, prompt wrapping and live server totals |

Tests use a deterministic fake Codex executable and local loopback connections. They make no OpenAI requests. The optional live examples are separate and consume model usage.

## Manual checks and limits

- Linux client and server demos were rendered and visually checked. [Screenshots](images/) contain sample data.
- Earlier live checks with the installed Codex verified prompt/reply streaming, model/effort selection, active-turn steering, session-local file creation and permanent thread deletion.
- Linux release binaries target glibc 2.31. Windows x64 binaries were cross-compiled; native Windows GUI and Codex sandbox behavior still need manual verification on Windows.
- Cleanup is confirmed through `thread/delete`, not archiving. Forced termination or power loss can delay deletion. The journal retries recorded IDs on startup, but native thread creation and local journaling cannot be atomic.
