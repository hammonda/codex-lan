//! A small authenticated TLS/WebSocket transport. Pairing is explicit and lasts
//! for one server run. Codex's own RPC listener is never exposed on the LAN.
use crate::{
    bridge::{BridgeConfig, CodexBridge},
    protocol::{ClientCommand, MAX_EVENT_BYTES, MAX_MESSAGE_BYTES, PROTOCOL_VERSION, ServerEvent},
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Semaphore, mpsc},
    task::JoinSet,
    time::{Instant, interval, sleep_until, timeout},
};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        handshake::server::{ErrorResponse, Request, Response},
        http::StatusCode,
        protocol::WebSocketConfig,
    },
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_CLIENTS: usize = 8;
const MAX_SESSIONS: usize = 8;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);
const PAIRING_PREFIX: &str = "codex-lan-v4:";

pub struct ServerConfig {
    pub bind: String,
    pub workspace: PathBuf,
    pub codex_path: PathBuf,
    pub state_dir: PathBuf,
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub enum MonitorEvent {
    Ready {
        address: String,
        pairing_code: String,
        workspace: String,
    },
    ClientConnected {
        id: String,
        peer: String,
        hostname: Option<String>,
    },
    ClientDisconnected {
        id: String,
    },
    SessionOpened {
        client_id: String,
        session_id: String,
        name: String,
        workspace: String,
        model: Option<String>,
        effort: Option<String>,
    },
    SessionUsage {
        session_id: String,
        params: serde_json::Value,
    },
    SessionClosed {
        session_id: String,
    },
    Log(String),
    Stopped,
}

pub struct ClientConfig {
    /// Socket address or DNS name with port, for example `192.168.1.10:7331`.
    pub address: String,
    pub pairing_code: String,
}

#[derive(Serialize, Deserialize)]
struct Pairing {
    version: u32,
    certificate: String,
    token: String,
}

struct Identity {
    acceptor: TlsAcceptor,
    pairing: Pairing,
}

impl Pairing {
    fn encode(&self) -> Result<String> {
        Ok(format!(
            "{PAIRING_PREFIX}{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(self)?)
        ))
    }

    fn decode(code: &str) -> Result<Self> {
        ensure!(code.len() <= 65_536, "Pairing code is too large");
        let encoded = code
            .trim()
            .strip_prefix(PAIRING_PREFIX)
            .context("Paste the complete pairing code shown on the server")?;
        let value: Self = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(encoded)
                .context("Invalid pairing code encoding")?,
        )
        .context("Invalid pairing code")?;
        ensure!(
            value.version == PROTOCOL_VERSION,
            "Client and server protocol versions differ"
        );
        ensure!(
            value.token.len() >= 32 && value.token.len() <= 256,
            "Invalid pairing token"
        );
        Ok(value)
    }

    fn connector(&self) -> Result<TlsConnector> {
        let der = URL_SAFE_NO_PAD
            .decode(&self.certificate)
            .context("Invalid paired certificate")?;
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from(der))
            .context("Invalid paired certificate")?;
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(TlsConnector::from(Arc::new(config)))
    }
}

fn make_identity() -> Result<Identity> {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["codex-lan".into()])?;
    let certificate = cert.der().clone();
    let key = PrivatePkcs8KeyDer::from(signing_key.serialize_der());
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], key.into())?;
    Ok(Identity {
        acceptor: TlsAcceptor::from(Arc::new(config)),
        pairing: Pairing {
            version: PROTOCOL_VERSION,
            certificate: URL_SAFE_NO_PAD.encode(certificate.as_ref()),
            token: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
        },
    })
}

fn websocket_config(receive_limit: usize) -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(receive_limit))
        .max_frame_size(Some(receive_limit))
        .max_write_buffer_size(MAX_EVENT_BYTES * 2)
}

#[allow(
    clippy::result_large_err,
    reason = "tungstenite's handshake contract requires an unboxed ErrorResponse"
)]
fn authenticate(
    request: &Request,
    response: Response,
    expected: &str,
) -> std::result::Result<Response, ErrorResponse> {
    let provided = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    // Compare the complete fixed-length random secret without early exit.
    let mut difference = provided.len() ^ expected.len();
    for (a, b) in provided.as_bytes().iter().zip(expected.as_bytes()) {
        difference |= usize::from(a ^ b);
    }
    let version_matches = request
        .headers()
        .get("x-codex-lan-version")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u32>().ok())
        == Some(PROTOCOL_VERSION);
    if difference != 0 || !version_matches || request.headers().contains_key("origin") {
        let mut failure = ErrorResponse::new(Some("Pairing failed".into()));
        *failure.status_mut() = StatusCode::UNAUTHORIZED;
        return Err(failure);
    }
    Ok(response)
}

// Informational only: session ownership always uses the server-assigned connection ID.
// Base64 keeps Unicode computer names safe in an optional HTTP upgrade header.
fn display_hostname(name: &str) -> Option<String> {
    let name = name.trim();
    (!name.is_empty()
        && name.len() <= 255
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_')))
    .then(|| name.to_owned())
}

fn request_hostname(request: &Request) -> Option<String> {
    let header = request
        .headers()
        .get("x-codex-lan-hostname")?
        .to_str()
        .ok()?;
    if header.len() > 340 {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(header).ok()?;
    display_hostname(std::str::from_utf8(&bytes).ok()?)
}

/// Runs until cancelled. Stopping closes clients, deletes their owned Codex
/// threads and shuts down the child process before returning.
pub async fn run_server(
    config: ServerConfig,
    events: mpsc::UnboundedSender<MonitorEvent>,
    stop: CancellationToken,
) -> Result<()> {
    let result = run_server_inner(config, &events, stop).await;
    let _ = events.send(MonitorEvent::Stopped);
    result
}

async fn run_server_inner(
    config: ServerConfig,
    events: &mpsc::UnboundedSender<MonitorEvent>,
    stop: CancellationToken,
) -> Result<()> {
    std::fs::create_dir_all(&config.workspace).context("Could not create workspace root")?;
    let workspace =
        std::fs::canonicalize(&config.workspace).context("Could not open workspace root")?;
    ensure!(workspace.is_dir(), "Workspace root must be a directory");
    let listener = TcpListener::bind(config.bind.trim())
        .await
        .context("Could not bind the LAN listener")?;
    let address = listener.local_addr()?.to_string();
    let identity = make_identity()?;
    let pairing_code = identity.pairing.encode()?;
    let token = Arc::new(identity.pairing.token);
    let bridge = CodexBridge::start(BridgeConfig {
        executable: config.codex_path,
        workspace: workspace.clone(),
        state_dir: config.state_dir,
        model: config.model,
    })
    .await
    .context("Could not start Codex or finish pending session cleanup")?;
    if stop.is_cancelled() {
        return bridge.shutdown().await;
    }
    let workspace = workspace.to_string_lossy().into_owned();
    let _ = events.send(MonitorEvent::Ready {
        address,
        pairing_code,
        workspace: workspace.clone(),
    });
    let client_slots = Arc::new(Semaphore::new(MAX_CLIENTS));
    let connection_stop = stop.child_token();
    let mut connections = JoinSet::new();
    let mut listener_error = None;
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            result = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = result {
                    let _ = events.send(MonitorEvent::Log(format!("Connection task failed: {error}")));
                }
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(value) => value,
                    Err(error) => { listener_error = Some(error); break; }
                };
                let Ok(permit) = client_slots.clone().try_acquire_owned() else { drop(stream); continue; };
                let acceptor = identity.acceptor.clone();
                let token = token.clone();
                let bridge = bridge.clone();
                let events = events.clone();
                let workspace = workspace.clone();
                let stop = connection_stop.child_token();
                connections.spawn(async move {
                    let _permit = permit;
                    let result = async {
                        stream.set_nodelay(true)?;
                        let handshake = async {
                            let tls = acceptor.accept(stream).await?;
                            let mut hostname = None;
                            #[allow(clippy::result_large_err, reason = "tungstenite requires an unboxed HTTP upgrade error response")]
                            let authenticate_client = |request: &Request, response: Response| {
                                let response = authenticate(request, response, &token)?;
                                hostname = request_hostname(request);
                                Ok(response)
                            };
                            let socket = tokio_tungstenite::accept_hdr_async_with_config(tls, authenticate_client, Some(websocket_config(MAX_MESSAGE_BYTES))).await?;
                            Ok::<_, anyhow::Error>((socket, hostname))
                        };
                        let (socket, hostname) = tokio::select! {
                            _ = stop.cancelled() => return Ok(()),
                            result = timeout(HANDSHAKE_TIMEOUT, handshake) => result.context("Connection handshake timed out")??,
                        };
                        serve_connection(socket, bridge, workspace, peer.to_string(), hostname, events.clone(), stop).await
                    }.await;
                    if let Err(error) = result {
                        let _ = events.send(MonitorEvent::Log(format!("Connection {peer}: {error:#}")));
                    }
                });
            }
        }
    }
    drop(listener);
    connection_stop.cancel();
    while let Some(result) = connections.join_next().await {
        if let Err(error) = result {
            let _ = events.send(MonitorEvent::Log(format!(
                "Connection task failed during cleanup: {error}"
            )));
        }
    }
    let shutdown = bridge.shutdown().await;
    if let Some(error) = listener_error {
        shutdown.context("Codex cleanup also failed after listener error")?;
        return Err(error).context("LAN listener failed");
    }
    shutdown
}

type OwnedSessions = Arc<Mutex<HashSet<String>>>;

fn owns(sessions: &OwnedSessions, id: &str) -> bool {
    sessions
        .lock()
        .expect("session ownership mutex poisoned")
        .contains(id)
}

fn command_session_id(command: &ClientCommand) -> Option<&str> {
    match command {
        ClientCommand::NewSession { .. } | ClientCommand::ListModels => None,
        ClientCommand::Prompt { session_id, .. }
        | ClientCommand::Interrupt { session_id }
        | ClientCommand::CloseSession { session_id }
        | ClientCommand::Reply { session_id, .. } => Some(session_id),
    }
}

async fn send_message<S>(socket: &mut WebSocketStream<S>, message: Message) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    timeout(WRITE_TIMEOUT, socket.send(message))
        .await
        .context("Peer stopped reading")??;
    Ok(())
}

async fn send_event<S>(socket: &mut WebSocketStream<S>, event: &ServerEvent) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let text = serde_json::to_string(event)?;
    ensure!(
        text.len() <= MAX_EVENT_BYTES,
        "Codex event exceeded transport size limit"
    );
    send_message(socket, Message::Text(text.into())).await
}

async fn serve_connection<S>(
    mut socket: WebSocketStream<S>,
    bridge: CodexBridge,
    workspace: String,
    peer: String,
    hostname: Option<String>,
    monitor: mpsc::UnboundedSender<MonitorEvent>,
    stop: CancellationToken,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let id = Uuid::new_v4().to_string();
    let sessions: OwnedSessions = Arc::new(Mutex::new(HashSet::new()));
    let mut notifications = bridge.subscribe();
    let (command_tx, command_rx) = mpsc::channel(16);
    let (result_tx, mut result_rx) = mpsc::channel(32);
    let worker_stop = stop.child_token();
    let worker = tokio::spawn(command_worker(
        bridge.clone(),
        sessions.clone(),
        command_rx,
        result_tx,
        monitor.clone(),
        id.clone(),
        worker_stop.clone(),
    ));
    let _ = monitor.send(MonitorEvent::ClientConnected {
        id: id.clone(),
        peer,
        hostname,
    });
    let result = async {
        send_event(&mut socket, &ServerEvent::Welcome { workspace, server_version: env!("CARGO_PKG_VERSION").into() }).await?;
        let mut heartbeat = interval(HEARTBEAT_INTERVAL);
        let mut usage_poll = interval(Duration::from_secs(60));
        let mut usage_task = None;
        let mut last_seen = Instant::now();
        loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => break,
                _ = usage_poll.tick(), if usage_task.is_none() => {
                    let bridge = bridge.clone();
                    usage_task = Some(Box::pin(async move { bridge.account_usage().await }));
                }
                usage = async { usage_task.as_mut().unwrap().await }, if usage_task.is_some() => {
                    usage_task = None;
                    send_event(&mut socket, &ServerEvent::AccountUsage { usage }).await?;
                }
                event = result_rx.recv() => {
                    let Some(event) = event else { bail!("Session worker stopped"); };
                    send_event(&mut socket, &event).await?;
                    if let ServerEvent::SessionStarted { session_id, .. } = &event {
                        // Publish buffered native creation events only after the
                        // client knows which session those events belong to.
                        bridge.publish_session(session_id)?;
                    }
                }
                _ = sleep_until(last_seen + HEARTBEAT_TIMEOUT) => bail!("Client heartbeat timed out"),
                _ = heartbeat.tick() => {
                    send_message(&mut socket, Message::Ping(Vec::new().into())).await?;
                }
                incoming = socket.next() => {
                    let Some(incoming) = incoming else { break; };
                    last_seen = Instant::now();
                    match incoming? {
                        Message::Text(text) => {
                            let command: ClientCommand = serde_json::from_str(&text).context("Invalid client command")?;
                            command_tx.try_send(command).context("Too many pending client commands")?;
                        }
                        Message::Ping(bytes) => send_message(&mut socket, Message::Pong(bytes)).await?,
                        Message::Pong(_) => {},
                        Message::Close(_) => break,
                        _ => bail!("Only JSON text messages are supported"),
                    }
                }
                event = notifications.recv() => {
                    let event = event.context("Codex event stream overflowed or closed; ending session")?;
                    // Lifecycle messages come from the command worker only, after ownership changes.
                    if matches!(event, ServerEvent::SessionStarted { .. } | ServerEvent::SessionClosed { .. }) { continue; }
                    if event.session_id().is_none_or(|session| owns(&sessions, session)) {
                        if let ServerEvent::NativeEvent { session_id, method, params } = &event
                            && method == "thread/tokenUsage/updated"
                        {
                            let _ = monitor.send(MonitorEvent::SessionUsage {
                                session_id: session_id.clone(),
                                params: params.clone(),
                            });
                        }
                        send_event(&mut socket, &event).await?;
                    }
                }
            }
        }
        Ok(())
    }.await;
    // Do not abort an in-flight thread/start: it must finish and be journalled
    // before we can know which new thread needs to be deleted.
    worker_stop.cancel();
    drop(command_tx);
    drop(result_rx);
    let worker_result = worker.await;
    let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
    let ids: Vec<_> = sessions
        .lock()
        .expect("session ownership mutex poisoned")
        .iter()
        .cloned()
        .collect();
    let mut cleanups = JoinSet::new();
    for session_id in ids {
        let bridge = bridge.clone();
        cleanups.spawn(async move {
            let result = bridge.close_session(&session_id).await;
            (session_id, result)
        });
    }
    while let Some(cleanup) = cleanups.join_next().await {
        match cleanup {
            Ok((session_id, Ok(()))) => {
                let _ = monitor.send(MonitorEvent::SessionClosed { session_id });
            }
            Ok((session_id, Err(error))) => {
                let _ = monitor.send(MonitorEvent::Log(format!(
                    "Cleanup pending for {session_id}: {error:#}"
                )));
            }
            Err(error) => {
                let _ = monitor.send(MonitorEvent::Log(format!("Cleanup task failed: {error}")));
            }
        }
    }
    let _ = monitor.send(MonitorEvent::ClientDisconnected { id });
    worker_result.context("Session worker failed")?;
    result
}

async fn command_worker(
    bridge: CodexBridge,
    sessions: OwnedSessions,
    mut commands: mpsc::Receiver<ClientCommand>,
    results: mpsc::Sender<ServerEvent>,
    monitor: mpsc::UnboundedSender<MonitorEvent>,
    client_id: String,
    stop: CancellationToken,
) {
    loop {
        let command = tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            command = commands.recv() => match command { Some(value) => value, None => break },
        };
        let session_id = command_session_id(&command).map(str::to_owned);
        let result = handle_command(&bridge, &sessions, command, &monitor, &client_id).await;
        let event = match result {
            Ok(event) => event,
            Err(error) => Some(ServerEvent::Error {
                session_id,
                message: format!("{error:#}"),
            }),
        };
        if let Some(event) = event {
            tokio::select! {
                biased;
                _ = stop.cancelled() => break,
                result = results.send(event) => if result.is_err() { break; },
            }
        }
    }
}

async fn handle_command(
    bridge: &CodexBridge,
    sessions: &OwnedSessions,
    command: ClientCommand,
    monitor: &mpsc::UnboundedSender<MonitorEvent>,
    client_id: &str,
) -> Result<Option<ServerEvent>> {
    if let Some(session_id) = command_session_id(&command) {
        ensure!(
            owns(sessions, session_id),
            "This session does not belong to this connection"
        );
    }
    match command {
        ClientCommand::ListModels => Ok(Some(ServerEvent::Models {
            models: bridge.refresh_models().await?,
            default_model: bridge.default_model(),
        })),
        ClientCommand::NewSession { name, options } => {
            ensure!(
                sessions
                    .lock()
                    .expect("session ownership mutex poisoned")
                    .len()
                    < MAX_SESSIONS,
                "Close a session before opening another (limit {MAX_SESSIONS})"
            );
            let name = name.trim();
            ensure!(
                !name.is_empty() && name.len() <= 160,
                "Session name must be 1–160 bytes"
            );
            let session_id = bridge.new_session_with_options(name, &options).await?;
            sessions
                .lock()
                .expect("session ownership mutex poisoned")
                .insert(session_id.clone());
            let workspace = bridge
                .session_workspace(&session_id)?
                .to_string_lossy()
                .into_owned();
            let (model, effort) = bridge.session_model_effort(&session_id)?;
            let _ = monitor.send(MonitorEvent::SessionOpened {
                client_id: client_id.into(),
                session_id: session_id.clone(),
                name: name.into(),
                workspace: workspace.clone(),
                model: model.clone(),
                effort: effort.clone(),
            });
            Ok(Some(ServerEvent::SessionStarted {
                session_id,
                name: name.into(),
                workspace,
                model,
                effort,
            }))
        }
        ClientCommand::Prompt { session_id, text } => {
            Ok(Some(match bridge.prompt(&session_id, &text).await {
                Ok(steered) => ServerEvent::PromptAccepted {
                    session_id,
                    steered,
                },
                Err(error) => ServerEvent::PromptRejected {
                    session_id,
                    text,
                    message: format!("{error:#}"),
                },
            }))
        }
        ClientCommand::Interrupt { session_id } => {
            bridge.interrupt(&session_id).await?;
            Ok(None)
        }
        ClientCommand::CloseSession { session_id } => {
            bridge.close_session(&session_id).await?;
            sessions
                .lock()
                .expect("session ownership mutex poisoned")
                .remove(&session_id);
            let _ = monitor.send(MonitorEvent::SessionClosed {
                session_id: session_id.clone(),
            });
            Ok(Some(ServerEvent::SessionClosed { session_id }))
        }
        ClientCommand::Reply {
            session_id,
            request_id,
            result,
        } => {
            bridge.reply(&session_id, &request_id, result).await?;
            Ok(None)
        }
    }
}

/// Opens a single connection. There is deliberately no automatic reconnect:
/// reconnecting creates new sessions, never revives an abandoned Codex thread.
pub async fn run_client(
    config: ClientConfig,
    mut commands: mpsc::UnboundedReceiver<ClientCommand>,
    events: mpsc::UnboundedSender<ServerEvent>,
    stop: CancellationToken,
) -> Result<()> {
    let pairing = Pairing::decode(&config.pairing_code)?;
    let connector = pairing.connector()?;
    let handshake = async {
        let tcp = TcpStream::connect(config.address.trim())
            .await
            .context("Could not reach the server address")?;
        tcp.set_nodelay(true)?;
        let tls = connector
            .connect(ServerName::try_from("codex-lan")?, tcp)
            .await
            .context("Server certificate does not match this pairing code")?;
        let mut request = "wss://codex-lan/".into_client_request()?;
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {}", pairing.token).parse()?,
        );
        request
            .headers_mut()
            .insert("x-codex-lan-version", PROTOCOL_VERSION.to_string().parse()?);
        if let Some(hostname) = display_hostname(&gethostname::gethostname().to_string_lossy()) {
            request.headers_mut().insert(
                "x-codex-lan-hostname",
                URL_SAFE_NO_PAD.encode(hostname).parse()?,
            );
        }
        let (socket, _) = tokio_tungstenite::client_async_with_config(
            request,
            tls,
            Some(websocket_config(MAX_EVENT_BYTES)),
        )
        .await
        .context("Server rejected pairing; copy the current code from its window")?;
        Ok::<_, anyhow::Error>(socket)
    };
    let mut socket = tokio::select! {
        _ = stop.cancelled() => return Ok(()),
        result = timeout(HANDSHAKE_TIMEOUT, handshake) => result.context("Server connection timed out")??,
    };
    let mut heartbeat = interval(HEARTBEAT_INTERVAL);
    let mut last_seen = Instant::now();
    let result = async {
        loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => break,
                _ = sleep_until(last_seen + HEARTBEAT_TIMEOUT) => bail!("Server heartbeat timed out; this connection cannot be resumed"),
                _ = heartbeat.tick() => {
                    send_message(&mut socket, Message::Ping(Vec::new().into())).await?;
                }
                incoming = socket.next() => {
                    let Some(incoming) = incoming else { bail!("Server disconnected; this connection cannot be resumed"); };
                    last_seen = Instant::now();
                    match incoming? {
                        Message::Text(text) => {
                            let event = serde_json::from_str(&text).context("Invalid server message")?;
                            events.send(event).map_err(|_| anyhow!("Client window closed"))?;
                        }
                        Message::Ping(bytes) => send_message(&mut socket, Message::Pong(bytes)).await?,
                        Message::Pong(_) => {},
                        Message::Close(_) => bail!("Server ended this connection"),
                        _ => bail!("Invalid server frame"),
                    }
                }
                command = commands.recv() => {
                    let Some(command) = command else { break; };
                    let text = serde_json::to_string(&command)?;
                    ensure!(text.len() <= MAX_MESSAGE_BYTES, "Command is too large");
                    send_message(&mut socket, Message::Text(text.into())).await?;
                }
            }
        }
        Ok(())
    }.await;
    let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_hostname_accepts_unicode_and_ignores_missing_or_invalid_metadata() {
        let mut request = Request::new(());
        assert_eq!(
            request_hostname(&request),
            None,
            "Older clients remain supported"
        );
        for name in ["WORKSTATION-01", "电脑.local", "  laptop_2  "] {
            request.headers_mut().insert(
                "x-codex-lan-hostname",
                URL_SAFE_NO_PAD.encode(name).parse().unwrap(),
            );
            assert_eq!(request_hostname(&request).as_deref(), Some(name.trim()));
        }
        for name in [
            "".into(),
            "bad\nname".into(),
            "bad\u{202e}name".into(),
            "a".repeat(256),
        ] {
            request.headers_mut().insert(
                "x-codex-lan-hostname",
                URL_SAFE_NO_PAD.encode(name).parse().unwrap(),
            );
            assert_eq!(request_hostname(&request), None);
        }
        request
            .headers_mut()
            .insert("x-codex-lan-hostname", "!invalid!".parse().unwrap());
        assert_eq!(request_hostname(&request), None);
    }

    #[test]
    fn pairing_round_trip_and_version_rejection() {
        let identity = make_identity().unwrap();
        let decoded = Pairing::decode(&identity.pairing.encode().unwrap()).unwrap();
        assert_eq!(decoded.token, identity.pairing.token);
        assert_eq!(decoded.certificate, identity.pairing.certificate);
        decoded.connector().unwrap();
        let unsupported = Pairing {
            version: 99,
            ..decoded
        };
        assert!(Pairing::decode(&unsupported.encode().unwrap()).is_err());
        assert!(Pairing::decode("not-a-pairing-code").is_err());
    }

    #[test]
    fn upgrade_requires_token_version_and_native_client() {
        let token = "abc123";
        let make_request = |secret: &str| {
            let mut request = Request::new(());
            request
                .headers_mut()
                .insert("authorization", format!("Bearer {secret}").parse().unwrap());
            request.headers_mut().insert(
                "x-codex-lan-version",
                PROTOCOL_VERSION.to_string().parse().unwrap(),
            );
            request
        };
        assert!(authenticate(&make_request(token), Response::new(()), token).is_ok());
        assert!(authenticate(&make_request("abc124"), Response::new(()), token).is_err());
        assert!(authenticate(&make_request("abc123x"), Response::new(()), token).is_err());
        let mut old_client = make_request(token);
        old_client
            .headers_mut()
            .insert("x-codex-lan-version", "1".parse().unwrap());
        assert!(authenticate(&old_client, Response::new(()), token).is_err());
        let mut request = make_request(token);
        request
            .headers_mut()
            .insert("origin", "https://untrusted.example".parse().unwrap());
        assert!(authenticate(&request, Response::new(()), token).is_err());
    }

    #[tokio::test]
    async fn tls_accepts_paired_certificate_and_rejects_another_server() {
        async fn attempt(trust_correct_server: bool) -> bool {
            let server = make_identity().unwrap();
            let other = make_identity().unwrap();
            let connector = if trust_correct_server {
                server.pairing.connector().unwrap()
            } else {
                other.pairing.connector().unwrap()
            };
            let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
            let task =
                tokio::spawn(async move { server.acceptor.accept(server_stream).await.is_ok() });
            let result = connector
                .connect(ServerName::try_from("codex-lan").unwrap(), client_stream)
                .await;
            let success = result.is_ok();
            drop(result);
            let _ = task.await;
            success
        }
        assert!(attempt(true).await);
        assert!(!attempt(false).await);
    }
}
