#![cfg(feature = "test-support")]

use codex_lan_core::transport::{MonitorEvent, ServerConfig, run_server};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::mpsc,
    task::JoinHandle,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

#[path = "stdio/delegation.rs"]
mod delegation;
#[path = "../../core/tests/support/mod.rs"]
mod fixture;

struct Server {
    temp: tempfile::TempDir,
    root: PathBuf,
    config: PathBuf,
    stop: CancellationToken,
    task: JoinHandle<anyhow::Result<()>>,
    _monitor: mpsc::UnboundedReceiver<MonitorEvent>,
}

impl Server {
    async fn start() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspaces");
        std::fs::create_dir(&root).unwrap();
        let (tx, mut monitor) = mpsc::unbounded_channel();
        let stop = CancellationToken::new();
        let task = tokio::spawn(run_server(
            ServerConfig {
                bind: "127.0.0.1:0".into(),
                workspace: root.clone(),
                codex_path: env!("CARGO_BIN_EXE_fake-codex-mcp").into(),
                state_dir: temp.path().join("state"),
                model: None,
            },
            tx,
            stop.clone(),
        ));
        let (address, pairing_code) = timeout(Duration::from_secs(10), async {
            loop {
                if let MonitorEvent::Ready {
                    address,
                    pairing_code,
                    ..
                } = monitor.recv().await.unwrap()
                {
                    break (address, pairing_code);
                }
            }
        })
        .await
        .unwrap();
        let config = temp.path().join("connection.json");
        std::fs::write(
            &config,
            json!({"address":address,"pairing_code":pairing_code}).to_string(),
        )
        .unwrap();
        Self {
            temp,
            root,
            config,
            stop,
            task,
            _monitor: monitor,
        }
    }

    fn prepare(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("TASK.md"), "Check prepared context — café").unwrap();
        path
    }

    async fn deleted(&self, id: &str) {
        wait_until(|| {
            fixture::calls(&self.root)
                .iter()
                .any(|call| call["method"] == "thread/delete" && call["params"]["threadId"] == id)
        })
        .await;
    }

    async fn finish(self) {
        self.stop.cancel();
        timeout(Duration::from_secs(10), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

struct Mcp {
    child: Child,
    input: Option<ChildStdin>,
    output: mpsc::UnboundedReceiver<Value>,
    buffered: HashMap<u64, Value>,
    next: u64,
}

impl Mcp {
    async fn start(config: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codex-lan-mcp"))
            .args(["--config"])
            .arg(config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let (tx, output) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(line) = lines.next_line().await.unwrap() {
                let message =
                    serde_json::from_str(&line).expect("stdout must contain only MCP JSON");
                if tx.send(message).is_err() {
                    break;
                }
            }
        });
        let mut mcp = Self {
            child,
            input,
            output,
            buffered: HashMap::new(),
            next: 1,
        };
        let id = mcp.request("initialize", json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"claude-test","version":"1"}})).await;
        let initialized = mcp.response(id).await;
        assert!(initialized.get("error").is_none(), "{initialized}");
        assert_eq!(initialized["result"]["serverInfo"]["name"], "codex-lan-mcp");
        mcp.notify("notifications/initialized", json!({})).await;
        mcp
    }

    async fn write(&mut self, value: Value) {
        let input = self.input.as_mut().unwrap();
        input
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
        input.flush().await.unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next;
        self.next += 1;
        self.write(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await;
        id
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.write(json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await;
    }

    async fn response(&mut self, id: u64) -> Value {
        if let Some(value) = self.buffered.remove(&id) {
            return value;
        }
        timeout(Duration::from_secs(15), async {
            loop {
                let value = self.output.recv().await.expect("MCP process closed stdout");
                if let Some(other) = value["id"].as_u64() {
                    if other == id {
                        return value;
                    }
                    self.buffered.insert(other, value);
                }
            }
        })
        .await
        .expect("MCP response timed out")
    }

    async fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let id = self
            .request("tools/call", json!({"name":tool,"arguments":arguments}))
            .await;
        self.response(id).await
    }

    async fn ok(&mut self, tool: &str, arguments: Value) -> Value {
        let response = self.call(tool, arguments).await;
        value(response)
    }

    async fn create(&mut self, path: &Path, name: &str) -> String {
        self.ok(
            "thread_create",
            json!({"name":name,"workspace":path,"model":"fixture-fast","effort":"medium"}),
        )
        .await["thread_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn until(&mut self, id: &str, status: &str) -> Value {
        timeout(Duration::from_secs(10), async {
            loop {
                let page = self.ok("thread_read", json!({"thread_id":id})).await;
                if page["thread"]["status"] == status {
                    return page;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }

    async fn eof(mut self) {
        self.input.take();
        let status = timeout(Duration::from_secs(5), self.child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success(), "MCP exit: {status}");
    }

    async fn kill(mut self) {
        self.child.kill().await.unwrap();
    }
}

fn value(response: Value) -> Value {
    assert!(response.get("error").is_none(), "{response}");
    assert_ne!(response["result"]["isError"], true, "{response}");
    assert!(
        response["result"].get("structuredContent").is_none(),
        "Do not duplicate text in structuredContent"
    );
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn is_error(response: &Value) -> bool {
    response.get("error").is_some() || response["result"]["isError"] == true
}

async fn wait_until(condition: impl Fn() -> bool) {
    timeout(Duration::from_secs(10), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prepared_context_models_output_and_explicit_deletion_work_over_stdio() {
    let server = Server::start().await;
    let mut mcp = Mcp::start(&server.config).await;
    let list = mcp.request("tools/list", json!({})).await;
    let tools = mcp.response(list).await;
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 10);
    let info = mcp.ok("server_info", json!({"catalog":true})).await;
    assert_eq!(
        info["server"]["workspace_root"],
        server
            .root
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(info["catalog"]["models"].as_array().unwrap().len(), 2);
    let prepared = server.prepare("prepared-雪");
    for workspace in [
        server.root.join("missing"),
        server.temp.path().join("outside"),
        server.root.clone(),
        server.root.join("../escape"),
    ] {
        let error = mcp
            .call(
                "thread_create",
                json!({"name":"invalid","workspace":workspace}),
            )
            .await;
        assert!(is_error(&error), "{error}");
    }
    assert!(!server.root.join("missing").exists());
    assert!(!server.temp.path().join("outside").exists());
    assert!(
        !fixture::calls(&server.root)
            .iter()
            .any(|call| call["method"] == "thread/start")
    );
    let id = mcp.create(&prepared, "Prepared task").await;
    let overlapping = mcp
        .call(
            "thread_create",
            json!({"name":"overlap","workspace":prepared.join("child")}),
        )
        .await;
    assert!(is_error(&overlapping));
    assert!(!prepared.join("child").exists());
    let sent = mcp
        .ok(
            "thread_send",
            json!({"thread_id":id,"text":"read prepared task"}),
        )
        .await;
    assert_eq!(sent["steered"], false);
    let page = mcp.until(&id, "completed").await;
    assert!(page.to_string().contains("Check prepared context"));
    assert_eq!(
        std::fs::read_to_string(prepared.join("result.txt")).unwrap(),
        "Check prepared context — café"
    );
    let closed = mcp.ok("thread_close", json!({"thread_id":id})).await;
    assert_eq!(closed["type"], "session_closed");
    server.deleted(&id).await;
    assert!(prepared.join("result.txt").is_file());
    assert!(
        mcp.ok("thread_list", json!({})).await["threads"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    mcp.eof().await;
    server.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_commands_steering_questions_and_dynamic_output_are_correlated() {
    let server = Server::start().await;
    let mut mcp = Mcp::start(&server.config).await;
    let a = mcp.request("tools/call", json!({"name":"thread_create","arguments":{"name":"a","workspace":server.prepare("a")}})).await;
    let b = mcp.request("tools/call", json!({"name":"thread_create","arguments":{"name":"b","workspace":server.prepare("b")}})).await;
    let second = value(mcp.response(b).await);
    let first = value(mcp.response(a).await);
    assert_eq!(first["name"], "Claude / a");
    assert_eq!(second["name"], "Claude / b");
    let id = first["thread_id"].as_str().unwrap();
    mcp.ok(
        "thread_send",
        json!({"thread_id":id,"text":"wait until interrupted"}),
    )
    .await;
    let working = mcp.until(id, "working").await;
    let waiting = mcp.request("tools/call", json!({"name":"thread_read","arguments":{"thread_id":id,"cursor":working["next_cursor"],"wait_ms":30000}})).await;
    let accepted = mcp
        .ok(
            "thread_send",
            json!({"thread_id":id,"text":"continue carefully"}),
        )
        .await;
    assert_eq!(accepted["steered"], true);
    mcp.notify(
        "notifications/cancelled",
        json!({"requestId":waiting,"reason":"stop waiting"}),
    )
    .await;
    assert_eq!(
        mcp.ok("thread_list", json!({})).await["threads"]
            .as_array()
            .unwrap()
            .iter()
            .find(|thread| thread["thread_id"] == id)
            .unwrap()["status"],
        "working"
    );
    mcp.ok("thread_interrupt", json!({"thread_id":id})).await;
    mcp.until(id, "interrupted").await;
    mcp.ok(
        "thread_send",
        json!({"thread_id":id,"text":"ask a question"}),
    )
    .await;
    let page = mcp.until(id, "needs_input").await;
    let request = &page["thread"]["pending_requests"][0]["request_id"];
    mcp.ok("thread_reply", json!({"thread_id":id,"request_id":request,"result":{"answers":{"choice":{"answers":["Linux"]}}}})).await;
    assert!(
        mcp.until(id, "completed")
            .await
            .to_string()
            .contains("Answer: Linux")
    );
    mcp.ok(
        "thread_send",
        json!({"thread_id":id,"text":"show native output"}),
    )
    .await;
    let page = mcp.until(id, "completed").await;
    assert_eq!(page["thread"]["usage"]["tokens"]["input"], 30);
    assert!(!page.to_string().contains("futureNotification"));
    let detailed = mcp
        .ok(
            "thread_read",
            json!({"thread_id":id,"detail":"everything","max_bytes":24000}),
        )
        .await;
    assert!(detailed.to_string().contains("futureNotification"));
    assert!(!detailed.to_string().contains("foreign-thread-marker"));
    assert_eq!(
        detailed,
        mcp.ok(
            "thread_read",
            json!({"thread_id":id,"detail":"everything","max_bytes":24000})
        )
        .await
    );
    let mut tail = detailed;
    while tail["has_more"] == true {
        tail = mcp
            .ok(
                "thread_read",
                json!({"thread_id":id,"cursor":tail["next_cursor"],"detail":"everything"}),
            )
            .await;
    }
    let drained = mcp
        .ok(
            "thread_read",
            json!({"thread_id":id,"cursor":tail["next_cursor"],"detail":"everything"}),
        )
        .await;
    assert!(drained["events"].as_array().unwrap().is_empty());
    mcp.eof().await;
    server.deleted(id).await;
    server.deleted(second["thread_id"].as_str().unwrap()).await;
    server.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn eof_and_forced_exit_clean_up_owned_threads_and_leave_other_clients_alone() {
    let server = Server::start().await;
    let mut first = Mcp::start(&server.config).await;
    let mut second = Mcp::start(&server.config).await;
    let id_a = first.create(&server.prepare("a"), "First client").await;
    let id_b = second.create(&server.prepare("b"), "Second client").await;
    for tool in ["thread_send", "thread_read", "thread_close"] {
        let mut args = json!({"thread_id":id_b});
        if tool == "thread_send" {
            args["text"] = json!("not owned");
        }
        assert!(is_error(&first.call(tool, args).await));
    }
    first
        .ok(
            "thread_send",
            json!({"thread_id":id_a,"text":"wait until interrupted"}),
        )
        .await;
    first.kill().await;
    server.deleted(&id_a).await;
    assert!(
        !fixture::calls(&server.root)
            .iter()
            .any(|call| call["method"] == "thread/delete" && call["params"]["threadId"] == id_b)
    );
    second
        .ok(
            "thread_send",
            json!({"thread_id":id_b,"text":"wait until interrupted"}),
        )
        .await;
    let active = second.until(&id_b, "working").await;
    second.request("tools/call", json!({"name":"thread_read","arguments":{"thread_id":id_b,"cursor":active["next_cursor"],"wait_ms":30000}})).await;
    second.eof().await;
    server.deleted(&id_b).await;
    assert!(server.root.join("a/TASK.md").is_file());
    server.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_creates_are_still_owned_and_disconnect_cleans_late_creation() {
    let server = Server::start().await;
    std::fs::write(server.root.join("delay-create"), "").unwrap();
    let mut mcp = Mcp::start(&server.config).await;
    let create = mcp.request("tools/call", json!({"name":"thread_create","arguments":{"name":"late","workspace":server.prepare("late")}})).await;
    wait_until(|| {
        fixture::calls(&server.root)
            .iter()
            .any(|call| call["method"] == "thread/start")
    })
    .await;
    mcp.notify(
        "notifications/cancelled",
        json!({"requestId":create,"reason":"cancel create wait"}),
    )
    .await;
    let id = timeout(Duration::from_secs(10), async {
        loop {
            let threads = mcp.ok("thread_list", json!({})).await;
            if let Some(id) = threads["threads"][0]["thread_id"].as_str() {
                break id.to_owned();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    mcp.request("tools/call", json!({"name":"thread_create","arguments":{"name":"disconnected","workspace":server.prepare("disconnected")}})).await;
    wait_until(|| {
        fixture::calls(&server.root)
            .iter()
            .filter(|call| call["method"] == "thread/start")
            .count()
            == 2
    })
    .await;
    mcp.eof().await;
    server.deleted(&id).await;
    wait_until(|| {
        fixture::calls(&server.root)
            .iter()
            .filter(|call| call["method"] == "thread/delete")
            .count()
            == 2
    })
    .await;
    server.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_native_output_is_paged_losslessly_and_invalid_calls_leave_connection_usable() {
    let server = Server::start().await;
    let mut mcp = Mcp::start(&server.config).await;
    let id = mcp.create(&server.prepare("large"), "Large output").await;
    let bad = mcp
        .call(
            "thread_send",
            json!({"thread_id":id,"text":"x".repeat(1024 * 1024)}),
        )
        .await;
    assert!(is_error(&bad));
    assert!(is_error(
        &mcp.call("thread_read", json!({"thread_id":id,"wait_ms":30001}))
            .await
    ));
    mcp.ok(
        "thread_send",
        json!({"thread_id":id,"text":"show large output"}),
    )
    .await;
    mcp.until(&id, "completed").await;
    let mut cursor = Value::Null;
    let mut records = HashMap::<u64, String>::new();
    loop {
        let page = mcp
            .ok(
                "thread_read",
                json!({"thread_id":id,"cursor":cursor,"detail":"everything","max_bytes":24000}),
            )
            .await;
        let mut bytes = 0;
        for event in page["events"].as_array().unwrap() {
            let record = records
                .entry(event["sequence"].as_u64().unwrap())
                .or_default();
            assert_eq!(record.len(), event["offset"].as_u64().unwrap() as usize);
            let part = event["text"].as_str().unwrap();
            bytes += part.len();
            record.push_str(part);
        }
        assert!(bytes <= 24000);
        cursor = page["next_cursor"].clone();
        if page["has_more"] == false {
            break;
        }
    }
    let large = records
        .values()
        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
        .find(|record| record["method"] == "item/commandExecution/outputDelta")
        .unwrap();
    assert_eq!(
        large["params"]["delta"].as_str().unwrap(),
        "x".repeat(2 * 1024 * 1024)
    );
    mcp.eof().await;
    server.deleted(&id).await;
    server.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deletion_failure_is_reported_and_server_shutdown_cannot_resume_threads() {
    let server = Server::start().await;
    let mut mcp = Mcp::start(&server.config).await;
    let prepared = server.prepare("retry");
    let id = mcp.create(&prepared, "Retry deletion").await;
    std::fs::write(server.root.join("fail-delete"), "").unwrap();
    assert!(is_error(
        &mcp.call("thread_close", json!({"thread_id":id})).await
    ));
    assert_eq!(
        mcp.ok("thread_list", json!({})).await["threads"][0]["thread_id"],
        id
    );
    std::fs::remove_file(server.root.join("fail-delete")).unwrap();
    mcp.ok("thread_close", json!({"thread_id":id})).await;
    let second = mcp.create(&prepared, "Server shutdown").await;
    mcp.ok(
        "thread_send",
        json!({"thread_id":second,"text":"wait until interrupted"}),
    )
    .await;
    server.stop.cancel();
    mcp.until(&second, "disconnected").await;
    assert!(is_error(
        &mcp.call(
            "thread_send",
            json!({"thread_id":second,"text":"must not reconnect"})
        )
        .await
    ));
    assert!(mcp.ok("thread_list", json!({})).await["connection_error"].is_string());
    server.deleted(&second).await;
    mcp.eof().await;
    server.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backend_failure_is_visible_even_while_the_lan_server_is_alive() {
    let server = Server::start().await;
    let mut mcp = Mcp::start(&server.config).await;
    let id = mcp
        .create(&server.prepare("backend"), "Backend failure")
        .await;
    mcp.call(
        "thread_send",
        json!({"thread_id":id,"text":"exit during turn"}),
    )
    .await;
    let page = timeout(Duration::from_secs(10), async {
        loop {
            let page = mcp.ok("thread_read", json!({"thread_id":id})).await;
            if !page["server_errors"].as_array().unwrap().is_empty() {
                break page;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        page["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "error")
    );
    assert!(is_error(
        &mcp.call("thread_close", json!({"thread_id":id})).await
    ));
    mcp.eof().await;
    server.stop.cancel();
    assert!(
        timeout(Duration::from_secs(10), server.task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairing_refresh_reconnects_the_same_adapter_without_resuming_or_replaying_old_work() {
    let mut first = Server::start().await;
    let mut mcp = Mcp::start(&first.config).await;
    let process = mcp.child.id();
    let old = mcp.create(&first.prepare("old"), "Old task").await;
    mcp.ok(
        "thread_send",
        json!({"thread_id":old,"text":"wait until interrupted"}),
    )
    .await;
    first.stop.cancel();
    mcp.until(&old, "disconnected").await;
    timeout(Duration::from_secs(10), &mut first.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    first.deleted(&old).await;

    let second = Server::start().await;
    let temporary = first.config.with_extension("new");
    std::fs::copy(&second.config, &temporary).unwrap();
    std::fs::rename(temporary, &first.config).unwrap();
    let info = mcp.ok("server_info", json!({"catalog":true})).await;
    assert_eq!(
        info["server"]["workspace_root"],
        json!(second.root.canonicalize().unwrap())
    );
    assert!(info["connection_error"].is_null());
    assert_eq!(
        mcp.child.id(),
        process,
        "MCP process must survive the LAN restart"
    );
    assert_eq!(
        mcp.ok("thread_read", json!({"thread_id":old})).await["thread"]["status"],
        "disconnected"
    );
    assert!(is_error(
        &mcp.call(
            "thread_send",
            json!({"thread_id":old,"text":"must never be replayed"})
        )
        .await
    ));
    let fresh = mcp.create(&second.prepare("fresh"), "Fresh task").await;
    mcp.ok(
        "thread_send",
        json!({"thread_id":fresh,"text":"fresh prompt"}),
    )
    .await;
    mcp.until(&fresh, "completed").await;
    let calls = fixture::calls(&second.root);
    let prompts: Vec<_> = calls
        .iter()
        .filter(|call| call["method"] == "turn/start")
        .collect();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0]["params"]["input"][0]["text"], "fresh prompt");
    mcp.eof().await;
    second.deleted(&fresh).await;
    second.finish().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn termination_signal_exits_with_stdin_open_and_deletes_owned_threads() {
    let server = Server::start().await;
    let mut mcp = Mcp::start(&server.config).await;
    let id = mcp
        .create(&server.prepare("signal"), "Signal cleanup")
        .await;
    mcp.ok(
        "thread_send",
        json!({"thread_id":id,"text":"wait until interrupted"}),
    )
    .await;
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(mcp.child.id().unwrap().to_string())
        .status()
        .await
        .unwrap();
    assert!(status.success());
    assert!(
        timeout(Duration::from_secs(5), mcp.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    server.deleted(&id).await;
    server.finish().await;
}
