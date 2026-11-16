#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() -> eframe::Result {
    let mut args = std::env::args_os().skip(1);
    let mut demo = false;
    let mut start = false;
    let mut connection_file = None;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--help" | "-h") => {
                println!(
                    "Codex LAN server\n\nUsage: codex-lan-server [--start] [--connection-file <path>] [--demo]\n\nSelect a workspace root, start the server, then pair a client.\nClient session directories stay inside this root.\nCodex CLI must be installed and signed in on this machine.\n--start  Start immediately using saved settings.\n--connection-file  Write a private JSON connection file for a local MCP client; refreshed on each server start. Keep it outside shared workspaces.\n--demo  Show a sample interface without starting a server."
                );
                return Ok(());
            }
            Some("--demo") => demo = true,
            Some("--start") => start = true,
            Some("--connection-file") => {
                let Some(path) = args.next() else {
                    eprintln!("--connection-file requires a path. Use --help.");
                    std::process::exit(2);
                };
                connection_file = Some(std::path::PathBuf::from(path));
            }
            _ => {
                eprintln!("Unknown argument: {}. Use --help.", arg.to_string_lossy());
                std::process::exit(2);
            }
        }
    }
    if demo && (start || connection_file.is_some()) {
        eprintln!("--demo cannot be combined with --start or --connection-file.");
        std::process::exit(2);
    }
    codex_lan_desktop::run_server(demo, start, connection_file)
}
