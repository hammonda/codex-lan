# Claude Code delegation

The local **Code tab** can launch `codex-lan-mcp` as a stdio MCP server. It connects to the same TLS LAN service as the egui client. Codex runs only on the server. This version accepts **prepared directories**; it does not upload files or prepare Git branches.

## Connect

Build the apps and adapter, or use their packaged binaries:

```sh
cargo build --release --locked -p codex-lan-desktop -p codex-lan-mcp
```

Run the updated server and copy its current pairing code into a private local `connection.json`:

```json
{
  "address": "192.168.1.10:7331",
  "pairing_code": "PASTE_THE_COMPLETE_CODE_FROM_THE_SERVER"
}
```

Keep this file outside your project repository. The code changes when the server restarts. Version 0.5 uses LAN protocol 5: rebuild/restart the server and any egui clients together.

When Claude runs on the server machine, the server can start immediately and maintain this file automatically (Linux example):

```sh
target/release/codex-lan-server --start --connection-file "$HOME/.config/codex-lan/connection.json"
```

Use that same path for the adapter's `--config`. The file uses a local address and owner-only permissions on Linux. The server's **Claude ↓** button saves the current pairing and remembers this file for automatic refresh on future starts. Without an override, it uses `~/.config/codex-lan/connection.json` (`$XDG_CONFIG_HOME` supported) or `%LOCALAPPDATA%\codex-lan\connection.json` on Windows. The adapter reloads the file and reconnects for new work after a restart.

Register the adapter for your Claude Code user account. Use absolute paths to the executable and config file:

```sh
claude mcp add --transport stdio --scope user codex-lan -- /absolute/path/codex-lan-mcp --config /absolute/path/connection.json
```

Windows PowerShell example:

```powershell
claude mcp add --transport stdio --scope user codex-lan -- "C:\Tools\CodexLAN\codex-lan-mcp.exe" --config "C:\Users\YOU\AppData\Local\codex-lan\connection.json"
```

Launch a new local Code session and check the `codex-lan` tools are connected. For a connection check without creating a thread, run the same adapter command with `--check`; it prints workspace/model/allowance metadata. `--help` lists options. Reconnection creates a fresh LAN connection: old threads remain disconnected and prompts are never resent. Reload an already-running MCP adapter once after upgrading its executable.

## Install the skill

From the repository or extracted package, Linux:

```sh
mkdir -p ~/.claude/skills/codex-delegate
cp skills/codex-delegate/SKILL.md ~/.claude/skills/codex-delegate/SKILL.md
```

Windows PowerShell:

```powershell
New-Item -ItemType Directory -Force "$env:USERPROFILE\.claude\skills\codex-delegate" | Out-Null
Copy-Item skills/codex-delegate/SKILL.md "$env:USERPROFILE\.claude\skills\codex-delegate\SKILL.md"
```

The personal skill is available across local projects, both automatically when relevant and as `/codex-delegate <subtask>`. For a project-specific delegation policy, add a short instruction to its `CLAUDE.md`, for example:

> Use codex-delegate for independent subtasks when useful. Supply context and acceptance criteria, then review and integrate results. Let the bridge handle waiting and cleanup.

## Prepare and delegate

If the allowed root is unknown, call `server_info`: `/data/scratch` by default on Linux, or `%USERPROFILE%` on Windows. Add `catalog: true` only to choose a model/effort; otherwise inherit defaults. Automatic desktop sessions use `codex-lan-threads/`; Claude can use other prepared directories inside the root. Existing installations keep their saved root; change **workspace root** in server settings and restart when needed.

If Claude shares the server filesystem, prepare a dedicated subdirectory there with the task's files and `TASK.md`. Otherwise arrange that directory on the server first. The bridge checks that it already exists, stays inside the root and does not overlap another active workspace. Codex's write access remains scoped to that selected directory.

Ask Claude, for example:

> Use codex-delegate to review the parser in `/data/scratch/parser-review`. Read TASK.md, write findings.md, and return a brief outcome and file path.

The preferred tool is one `delegate` call:

```json
{"name":"Parser review","workspace":"/data/scratch/parser-review","text":"Read TASK.md and write findings.md."}
```

Rust creates the native thread, submits the assignment, waits for local events, collects the final answer and confirms native deletion. It appends a brief handoff instruction asking for file deliverables and a short final reply. Questions return immediately; `thread_reply` answers and waits again. Failures and unconfirmed cleanup are reported explicitly. Independent calls can run concurrently; no model-based scheduler or summarizer is involved.

For manual multi-turn work, use `thread_create`, `thread_send`, `thread_wait`, then `thread_close`. `thread_send` also steers active work. `thread_interrupt` stops a turn. `thread_list` is for recovery after an uncertain/cancelled call, not periodic monitoring.

## Token use and monitoring

- The **server inspector** still receives the full native feed, including commentary, reasoning, commands, results and usage. Its dynamic detail controls remain available, including for recently closed sessions.
- **Claude receives** final text, questions or actionable failures. Progress, raw events, repeated usage snapshots and the model catalog are excluded from normal delegation results. Explicit `final_answer` messages are selected; older native versions without phases fall back to the last completed message of the accepted turn.
- **Rust waits** on the existing LAN event stream. It sends optional numeric MCP progress heartbeats every 20 seconds, never transcript text. Waiting, status handling, filtering and cleanup make no model inference calls. Claude-authored polling calls and their results would consume Claude context; the updated skill avoids them. Actual Codex work and Claude's assignment/result interpretation still consume their respective model tokens.
- Replies default to **4000 UTF-8 bytes**. Longer answers report `next_offset` and their full byte length; `thread_wait` can retrieve another slice (up to 24000 bytes) if needed. Nothing is silently truncated; files remain the preferred home for large deliverables. Questions are returned intact.
- `thread_read` remains **opt-in diagnostics** with `replies`, `progress`, `tools` or `everything` pages. Returning these to Claude spends context; use the server UI when only a human needs them. Responses use one compact JSON text block instead of repeating it in `structuredContent`; host tokenization is host-dependent.

Long calls use ordinary MCP tools compatible with the Desktop Code tab. Claude Code may background them and deliver the result when ready. Cancelling a wait does not stop delegated work or its automatic cleanup; recover with `thread_list` and `thread_wait`, or interrupt explicitly. This does not depend on MCP tasks, which the [current Claude documentation](https://code.claude.com/docs/en/mcp#mcp-tasks-that-run-on-the-server) says are unavailable in Desktop. Host tool timeouts still apply; reconnect the adapter after upgrading and reload the skill in a new Code session.

## Lifetime and limits

Each adapter process owns only its own threads. Server labels start with `Claude /` and retain the existing client hostname/IP display. Explicit close waits for deletion confirmation. EOF, process exit, termination signals and lost LAN connections trigger server cleanup; loss detection uses the 30-second heartbeat timeout. Failed deletions remain in the retry journal. **Working files remain.**

A visible tab or window can close while Claude keeps its session alive. Delegations close automatically when their turn ends; manually created threads need explicit close or MCP disconnection. Native Code-tab close/archive behavior should be checked with your installed Desktop version; automated tests exercise stdio EOF and process termination.

Limits: 8 clients, 8 live threads per connection, 256 KiB prompts including handoff text. `thread_wait` has no periodic timeout response; diagnostic `thread_read` waits are limited to 30 seconds. Diagnostic history is bounded to 64 MiB/20,000 records per thread, with at most 8 disconnected threads retained. Expired cursors are explicit errors. Automatic close retains the last 8 delegation results in adapter memory until exit; manual close releases its transcript. Disconnects never replay prompts. Working files persist.

References: [Claude Code MCP](https://code.claude.com/docs/en/mcp), [personal skills](https://code.claude.com/docs/en/skills#choose-where-skills-load).
