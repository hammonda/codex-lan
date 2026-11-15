#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "Codex LAN client\n\nUsage: codex-lan-client [--demo]\n\nConnect using the server's LAN address and pairing code.\nUse + to choose a session model, effort and server folder.\nCtrl+Enter sends a prompt, or steers Codex while it is working.\n--demo  Show a sample interface without connecting to a server."
        );
        return Ok(());
    }
    if let Some(arg) = args.iter().find(|a| a.as_str() != "--demo") {
        eprintln!("Unknown argument: {arg}. Use --help.");
        std::process::exit(2);
    }
    codex_lan_desktop::run_client(args.iter().any(|a| a == "--demo"))
}
