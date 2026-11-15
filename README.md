# Codex LAN

Rust/egui desktop client and server for running Codex across a LAN, on Linux or Windows. The server uses its locally installed, signed-in `codex app-server`; it does not call the OpenAI model API directly. The client needs no Codex installation or API key.

[Client preview](docs/images/client.png) · [Server preview](docs/images/server.png) · [Verification](docs/VERIFICATION.md)

## Build and run

Use Rust 1.95 or later. Linux needs a C compiler and the desktop libraries listed below; Windows needs the MSVC toolchain and Visual Studio C++ Build Tools.

```sh
git clone https://github.com/hammonda/codex-lan.git
cd codex-lan
cargo build --release --locked
```

Run `target/release/codex-lan-server` on the server computer and `target/release/codex-lan-client` on the client computer (add `.exe` on Windows). Both support `--help` and `--demo`; demo mode uses sample data without connecting to Codex.

1. Install Codex CLI on the server and run `codex login`. It must support `thread/delete`.
2. Start the server application, choose its workspace root, and click **start**. The default port is **7331**; allow it through the server's LAN firewall.
3. In the client, enter the server's LAN address and copy its pairing code. Traffic uses TLS; share the code only with intended clients. Restarting the server creates a new code.
4. Use **+** to create a session. Choose its model, effort and optional working folder, or inherit the defaults.
5. Type a prompt and press **Enter**. Prompts sent while Codex is working steer the active turn.

## Controls and output

| Control | Action |
| --- | --- |
| Enter / Shift+Enter | Send / insert a newline; Ctrl+Enter also sends |
| + / × | Create / close a session; closing deletes its native Codex thread |
| Arrow / JSON | Expand available output / inspect raw native events |
| … / copy | Show complete long output / copy the original text |
| Stop | Interrupt the active turn |

Switch output levels at any time: **Replies** shows response text; **Progress** adds plans, emitted reasoning and activity headings; **Tools** adds tool output and diffs; **Everything** shows the native event feed. Received data remains available when switching levels. JSON objects have collapsible levels and right-click copying. Display controls make no extra model calls.

The server shows each session's client computer name, LAN address, model, effort and working folder. Both apps show cumulative input/output tokens and an API-equivalent cost estimate in amber, updated when Codex reports usage. Hover for exact counts and assumptions. Pricing is a bundled snapshot dated **15 November 2026** from [OpenAI's API pricing](https://developers.openai.com/api/docs/pricing); it is not a Codex subscription bill. Unknown counts or prices show `—`.

The client's **week** badge shows the account's reported weekly allowance remaining. It refreshes from native account events and periodic metadata queries, without model inference.

## Workspaces and session lifetime

- The default server workspace root is `/data/scratch/codex-lan-threads` on Linux or `%USERPROFILE%\codex-lan-threads` on Windows. Each session gets a unique subfolder. Clients may override it only with a folder inside that root; live session folders cannot overlap.
- Codex gets write access to the session folder and internet access, subject to host policies. Displayed paths refer to the **server's filesystem**; files are not automatically transferred to the client.
- Threads are named `LAN / <session name>`. Closing a session or client, losing its connection, or stopping the server interrupts affected turns and permanently deletes their native threads. Other clients' sessions remain separate. **Files remain on disk.** There is no session restore or persistent client transcript.
- Failed deletions are recorded and retried on the next server start. A crash or forced termination can delay cleanup. Keep the state directory outside session workspaces: `$XDG_STATE_HOME/codex-lan` or `~/.local/state/codex-lan` on Linux; `%LOCALAPPDATA%\codex-lan` on Windows.

Use matching app builds when upgrading. Protocol 4 clients remain compatible; older clients report only their IP address. Restarting either app ends its affected sessions.

## Development

Linux build dependencies (Debian/Ubuntu):

```sh
sudo apt-get install build-essential pkg-config libx11-dev libxi-dev libxcursor-dev libxrandr-dev libxinerama-dev libxkbcommon-dev libwayland-dev libgl1-mesa-dev
```

Run the checks used by [CI](https://github.com/hammonda/codex-lan/actions):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
```

Tests use a local fake Codex process, including loopback TLS connections; they require no account or model calls. The examples under `crates/core/examples` are optional **live** checks and can consume Codex allowance.

`crates/core` contains the Codex bridge, LAN transport and protocol. `crates/desktop/src/client.rs` handles client state; `client/view.rs` and `client/transcript.rs` render it. `server.rs` contains the server dashboard; shared modules implement the theme, JSON viewer, prompt and usage displays.

For distribution, build the portable targets with Zig/cargo-zigbuild and the MinGW cross toolchain, then package:

```sh
cargo zigbuild --release -p codex-lan-desktop --target x86_64-unknown-linux-gnu.2.31 --target-dir target/portable-linux
cargo build --release -p codex-lan-desktop --target x86_64-pc-windows-gnu --target-dir target/windows
python3 tools/package.py
```

This writes Linux and Windows archives, source and checksums to `dist/`. The Linux archive requires glibc 2.31+, an X11/Wayland desktop and OpenGL. CI also builds executables on Linux and Windows and uploads them as workflow artifacts.

MIT licensed. Protocol reference: [Codex app-server](https://learn.chatgpt.com/docs/app-server).
