use anyhow::{Context, Result, bail};
use codex_lan_mcp::{Client, McpServer};
use rmcp::ServiceExt;
use std::{
    path::PathBuf,
    pin::Pin,
    task::{Context as TaskContext, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, ReadBuf};
use tokio_util::sync::CancellationToken;

fn main() {
    // Tokio's stdin reader can block indefinitely after a signal. Do not wait for
    // its blocking thread when the MCP host has already asked us to exit.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Create async runtime");
    let result = runtime.block_on(run());
    runtime.shutdown_timeout(Duration::from_millis(100));
    if let Err(error) = result {
        eprintln!("codex-lan-mcp: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let mut path = None;
    let mut check = false;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--help" | "-h") => {
                println!(
                    "codex-lan-mcp --config <connection.json> [--check]\n\nStdio MCP adapter for Claude Code. JSON config: address and pairing_code.\n--check connects and prints server metadata without creating a thread.\nUse --version to print the build version. Diagnostics go to stderr."
                );
                return Ok(());
            }
            Some("--version" | "-V") => {
                println!("codex-lan-mcp {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            Some("--config") => {
                path = Some(PathBuf::from(
                    args.next().context("--config requires a path")?,
                ));
            }
            Some("--check") => check = true,
            _ => bail!("Unknown argument; see --help"),
        }
    }
    let path =
        path.context("Pass --config with an absolute path to connection.json; see --help")?;
    let client = Client::from_config_file(path)?;
    if check {
        let info = client.info().await;
        client.shutdown().await;
        println!("{}", serde_json::to_string_pretty(&info?)?);
        return Ok(());
    }
    let stop = CancellationToken::new();
    let signal_stop = stop.clone();
    let signal = tokio::spawn(async move {
        shutdown_signal().await;
        signal_stop.cancel();
    });
    let reader = EndAwareReader {
        inner: tokio::io::stdin(),
        stop: stop.clone(),
    };
    let service = tokio::select! {
        _ = stop.cancelled() => None,
        result = McpServer::new(client.clone()).serve((reader, tokio::io::stdout())) => Some(result),
    };
    let outcome = match service {
        Some(Ok(service)) => {
            let cancel = service.cancellation_token();
            let outcome = tokio::select! {
                _ = stop.cancelled() => Ok(()),
                result = service.waiting() => result.map(|_| ()).context("MCP worker stopped"),
            };
            cancel.cancel();
            outcome
        }
        Some(Err(error)) => Err(anyhow::anyhow!("MCP initialization failed: {error}")),
        None => Ok(()),
    };
    // EOF/signals close the LAN socket immediately, even if a tool call is waiting.
    // The server owns deletion, so its cleanup continues if the host kills us.
    client.shutdown().await;
    signal.abort();
    outcome
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

struct EndAwareReader<R> {
    inner: R,
    stop: CancellationToken,
}

impl<R: AsyncRead + Unpin> AsyncRead for EndAwareReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let had_capacity = buf.remaining() > 0;
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(&result, Poll::Ready(Err(_)))
            || (matches!(&result, Poll::Ready(Ok(())))
                && had_capacity
                && buf.filled().len() == before)
        {
            this.stop.cancel();
        }
        result
    }
}
