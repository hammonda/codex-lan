#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "Codex LAN server\n\nUsage: codex-lan-server [--demo]\n\nSelect a workspace root, start the server, then pair a client.\nClient session directories stay inside this root.\nCodex CLI must be installed and signed in on this machine.\n--demo  Show a sample interface without starting a server."
        );
        return Ok(());
    }
    if let Some(arg) = args.iter().find(|a| a.as_str() != "--demo") {
        eprintln!("Unknown argument: {arg}. Use --help.");
        std::process::exit(2);
    }
    codex_lan_desktop::run_server(args.iter().any(|a| a == "--demo"))
}
