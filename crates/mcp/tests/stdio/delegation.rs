use super::*;
use codex_lan_core::protocol::ServerEvent;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn noisy_work_returns_only_final_result_and_keeps_human_trace() {
    let mut server = Server::start().await;
    let mut mcp = Mcp::start(&server.config).await;
    let prepared = server.prepare("noise");
    let info = mcp.ok("server_info", json!({})).await;
    assert!(info["workspace_root"].is_string());
    assert!(info.get("catalog").is_none());
    assert!(info.to_string().len() < 256);

    // Measure the previous default read path against the same deterministic workload.
    let manual = mcp.create(&prepared, "Baseline").await;
    mcp.ok(
        "thread_send",
        json!({"thread_id":manual,"text":"noisy delegation"}),
    )
    .await;
    let final_only = mcp.ok("thread_wait", json!({"thread_id":manual})).await;
    assert_eq!(final_only["reply"], "Done. Tests pass; see result.txt.");
    let mut cursor = None;
    let mut baseline_bytes = 0;
    let mut baseline_reads = 0;
    loop {
        let page = mcp
            .ok("thread_read", json!({"thread_id":manual,"cursor":cursor}))
            .await;
        baseline_bytes += page.to_string().len();
        baseline_reads += 1;
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if page["has_more"] != true {
            break;
        }
    }
    mcp.ok("thread_close", json!({"thread_id":manual})).await;

    let result = mcp
        .ok(
            "delegate",
            json!({"name":"Quiet delegation","workspace":prepared,"text":"noisy delegation"}),
        )
        .await;
    let id = result["thread_id"].as_str().unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["closed"], true);
    assert_eq!(result["reply"], "Done. Tests pass; see result.txt.");
    assert!(!result.to_string().contains("HUMAN-ONLY"));
    assert!(result.to_string().len() < 300);
    assert!(baseline_bytes > 100_000 && baseline_bytes > result.to_string().len() * 100);
    assert!(baseline_reads > 10);
    eprintln!(
        "Payload benchmark: previous replies={baseline_bytes} bytes/{baseline_reads} read calls; delegate={} bytes/1 call",
        result.to_string().len()
    );
    server.deleted(id).await;
    let mut commentary = false;
    let mut tool_output = false;
    while let Ok(event) = server._monitor.try_recv() {
        if let MonitorEvent::SessionEvent(ServerEvent::NativeEvent {
            session_id,
            method,
            params,
        }) = event
            && session_id == id
        {
            commentary |= params.to_string().contains("HUMAN-ONLY-PROGRESS");
            tool_output |= method == "item/commandExecution/outputDelta";
        }
    }
    assert!(
        commentary && tool_output,
        "Human inspector must still receive all native output"
    );
    let calls = fixture::calls(&server.root);
    for method in ["thread/start", "turn/start", "thread/delete"] {
        assert_eq!(
            calls.iter().filter(|c| c["method"] == method).count(),
            2,
            "{method}"
        );
    }
    assert_eq!(mcp.ok("thread_wait", json!({"thread_id":id})).await, result);
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
async fn delegated_question_returns_intact_then_reply_waits_and_cleans_up() {
    let server = Server::start().await;
    let mut mcp = Mcp::start(&server.config).await;
    let question = mcp.ok("delegate", json!({"name":"Question","workspace":server.prepare("question"),"text":"ask a question"})).await;
    assert_eq!(question["status"], "needs_input");
    let id = &question["thread_id"];
    let request = &question["requests"][0];
    assert_eq!(
        request["params"]["questions"][0]["question"],
        "Which target?"
    );
    assert!(
        !fixture::calls(&server.root)
            .iter()
            .any(|c| c["method"] == "thread/delete")
    );
    let result = mcp.ok("thread_reply", json!({"thread_id":id,"request_id":request["request_id"],"result":{"answers":{"choice":{"answers":["Windows"]}}}})).await;
    assert_eq!(result["reply"], "Answer: Windows", "{result}");
    assert_eq!(result["closed"], true);
    server.deleted(id.as_str().unwrap()).await;
    mcp.eof().await;
    server.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_delegate_still_finishes_once_and_retains_its_result() {
    let server = Server::start().await;
    std::fs::write(server.root.join("delay-create"), "").unwrap();
    let mut mcp = Mcp::start(&server.config).await;
    let call = mcp.request("tools/call", json!({"name":"delegate","arguments":{"name":"Cancelled wait","workspace":server.prepare("cancelled"),"text":"read prepared task"}})).await;
    wait_until(|| {
        fixture::calls(&server.root)
            .iter()
            .any(|c| c["method"] == "thread/start")
    })
    .await;
    mcp.notify("notifications/cancelled", json!({"requestId":call}))
        .await;
    wait_until(|| {
        fixture::calls(&server.root)
            .iter()
            .any(|c| c["method"] == "thread/delete")
    })
    .await;
    let id = timeout(Duration::from_secs(5), async {
        loop {
            let list = mcp.ok("thread_list", json!({})).await;
            if let Some(id) = list["results"][0]["thread_id"].as_str() {
                break id.to_owned();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let result = mcp.ok("thread_wait", json!({"thread_id":id})).await;
    assert_eq!(result["closed"], true);
    assert!(
        result["reply"]
            .as_str()
            .unwrap()
            .contains("Check prepared context")
    );
    for method in ["thread/start", "turn/start", "thread/delete"] {
        assert_eq!(
            fixture::calls(&server.root)
                .iter()
                .filter(|c| c["method"] == method)
                .count(),
            1
        );
    }
    assert!(server.root.join("cancelled/result.txt").exists());
    mcp.eof().await;
    server.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn long_final_answers_are_bounded_and_recoverable_after_native_deletion() {
    let server = Server::start().await;
    let mut mcp = Mcp::start(&server.config).await;
    let first = mcp.ok("delegate", json!({"name":"Long answer","workspace":server.prepare("long"),"text":"long final answer"})).await;
    assert_eq!(first["closed"], true);
    let mut text = first["reply"].as_str().unwrap().to_owned();
    assert!(text.len() <= 4000);
    let id = &first["thread_id"];
    let rest = mcp
        .ok(
            "thread_wait",
            json!({"thread_id":id,"offset":first["next_offset"],"max_bytes":24000}),
        )
        .await;
    text.push_str(rest["reply"].as_str().unwrap());
    assert_eq!(text, "résumé🙂\n".repeat(1500));
    assert!(rest.get("next_offset").is_none());
    assert!(is_error(
        &mcp.call("thread_wait", json!({"thread_id":id,"offset":2}))
            .await
    ));
    assert!(is_error(
        &mcp.call("thread_wait", json!({"thread_id":id,"max_bytes":25000}))
            .await
    ));
    mcp.eof().await;
    server.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn independent_waiters_ignore_activity_and_allow_steering_and_interrupt() {
    let server = Server::start().await;
    let mut mcp = Mcp::start(&server.config).await;
    let a = mcp.request("tools/call", json!({"name":"delegate","arguments":{"name":"a","workspace":server.prepare("a"),"text":"wait until interrupted"}})).await;
    let b = mcp.request("tools/call", json!({"name":"delegate","arguments":{"name":"b","workspace":server.prepare("b"),"text":"wait until interrupted"}})).await;
    wait_until(|| {
        fixture::calls(&server.root)
            .iter()
            .filter(|c| c["method"] == "turn/start")
            .count()
            == 2
    })
    .await;
    let list = mcp.ok("thread_list", json!({})).await;
    assert!(!mcp.buffered.contains_key(&a) && !mcp.buffered.contains_key(&b));
    let threads = list["threads"].as_array().unwrap();
    let id_a = &threads.iter().find(|t| t["name"] == "Claude / a").unwrap()["thread_id"];
    let id_b = &threads.iter().find(|t| t["name"] == "Claude / b").unwrap()["thread_id"];
    mcp.ok("thread_interrupt", json!({"thread_id":id_a})).await;
    let result_a = value(mcp.response(a).await);
    assert_eq!(result_a["status"], "interrupted");
    assert_eq!(result_a["closed"], true);
    assert!(!mcp.buffered.contains_key(&b));
    mcp.ok(
        "thread_send",
        json!({"thread_id":id_b,"text":"finish after steering"}),
    )
    .await;
    let result_b = value(mcp.response(b).await);
    assert_eq!(result_b["status"], "completed");
    assert_eq!(result_b["closed"], true);
    mcp.eof().await;
    server.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delegation_reports_cleanup_failure_without_losing_successful_output() {
    let server = Server::start().await;
    std::fs::write(server.root.join("fail-delete"), "").unwrap();
    let mut mcp = Mcp::start(&server.config).await;
    let result = mcp.ok("delegate", json!({"name":"Cleanup failure","workspace":server.prepare("cleanup"),"text":"read prepared task"})).await;
    assert_eq!(result["status"], "completed");
    assert_eq!(result["closed"], false);
    assert!(result["cleanup_error"].as_str().unwrap().contains("delete"));
    assert!(result["reply"].is_string());
    std::fs::remove_file(server.root.join("fail-delete")).unwrap();
    mcp.ok("thread_close", json!({"thread_id":result["thread_id"]}))
        .await;
    assert_eq!(
        mcp.ok("thread_wait", json!({"thread_id":result["thread_id"]}))
            .await["closed"],
        true
    );
    mcp.eof().await;
    server.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delegation_wakes_on_backend_failure_and_reports_uncertain_cleanup() {
    let server = Server::start().await;
    let mut mcp = Mcp::start(&server.config).await;
    let result = mcp.ok("delegate", json!({"name":"Backend exit","workspace":server.prepare("backend-exit"),"text":"exit during turn"})).await;
    assert!(matches!(
        result["status"].as_str(),
        Some("backend_failed" | "submission_failed")
    ));
    assert_eq!(result["closed"], false);
    assert!(result["error"].is_string() && result["cleanup_error"].is_string());
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
async fn delegation_wakes_on_disconnect_without_reconnecting_or_replaying_work() {
    let server = Server::start().await;
    let mut mcp = Mcp::start(&server.config).await;
    let call = mcp.request("tools/call", json!({"name":"delegate","arguments":{"name":"Disconnected","workspace":server.prepare("disconnected"),"text":"wait until interrupted"}})).await;
    wait_until(|| {
        fixture::calls(&server.root)
            .iter()
            .any(|c| c["method"] == "turn/start")
    })
    .await;
    server.stop.cancel();
    let result = value(mcp.response(call).await);
    assert_eq!(result["status"], "disconnected");
    assert_eq!(result["closed"], false);
    assert!(
        result["cleanup_error"]
            .as_str()
            .unwrap()
            .contains("unconfirmed")
    );
    assert_eq!(
        fixture::calls(&server.root)
            .iter()
            .filter(|c| c["method"] == "turn/start")
            .count(),
        1
    );
    mcp.eof().await;
    server.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completed_delegations_keep_only_eight_results_and_reject_invalid_prompts_before_creation()
{
    let server = Server::start().await;
    let mut mcp = Mcp::start(&server.config).await;
    let prepared = server.prepare("bounded");
    let mut first = Value::Null;
    for text in [String::new(), "x".repeat(256 * 1024)] {
        assert!(is_error(
            &mcp.call(
                "delegate",
                json!({"name":"Invalid","workspace":prepared,"text":text})
            )
            .await
        ));
    }
    assert!(
        !fixture::calls(&server.root)
            .iter()
            .any(|c| c["method"] == "thread/start")
    );
    for n in 0..9 {
        let result = mcp
            .ok(
                "delegate",
                json!({"name":format!("Job {n}"),"workspace":prepared,"text":"read prepared task"}),
            )
            .await;
        if n == 0 {
            first = result["thread_id"].clone();
        }
        assert_eq!(result["closed"], true);
    }
    let list = mcp.ok("thread_list", json!({})).await;
    assert_eq!(list["results"].as_array().unwrap().len(), 8);
    assert!(list["results"][0]["name"].is_string());
    assert!(list["threads"].as_array().unwrap().is_empty());
    assert!(is_error(
        &mcp.call("thread_wait", json!({"thread_id":first})).await
    ));
    mcp.eof().await;
    server.finish().await;
}
