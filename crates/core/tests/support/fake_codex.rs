//! Deterministic app-server fixture. Never connects to OpenAI or reads Codex credentials.
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, Write};

fn send(value: Value) {
    println!("{value}");
    std::io::stdout().flush().unwrap();
}

fn complete(thread: &str, turn: &str, status: &str) {
    send(
        json!({"method":"turn/completed","params":{"threadId":thread,"turn":{"id":turn,"status":status,"items":[]}}}),
    );
}

fn native_output(thread: &str, turn: &str) {
    let command = json!({"id":"command-1","type":"commandExecution","command":"printf 'native output'","cwd":"/fixture","status":"inProgress","aggregatedOutput":""});
    send(
        json!({"method":"item/started","params":{"threadId":thread,"turnId":turn,"item":command}}),
    );
    for delta in ["native ", "output\n"] {
        send(
            json!({"method":"item/commandExecution/outputDelta","params":{"threadId":thread,"turnId":turn,"itemId":"command-1","delta":delta}}),
        );
    }
    send(
        json!({"method":"item/completed","params":{"threadId":thread,"turnId":turn,"item":{"id":"command-1","type":"commandExecution","command":"printf 'native output'","cwd":"/fixture","status":"completed","aggregatedOutput":"native output\n","exitCode":0}}}),
    );
    send(
        json!({"method":"turn/plan/updated","params":{"threadId":thread,"turnId":turn,"explanation":"Inspect then edit","plan":[{"step":"Inspect workspace","status":"completed"},{"step":"Edit file","status":"inProgress"}]}}),
    );
    send(
        json!({"method":"turn/diff/updated","params":{"threadId":thread,"turnId":turn,"diff":"--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-before\n+after\n"}}),
    );
    send(
        json!({"method":"item/reasoning/summaryTextDelta","params":{"threadId":thread,"turnId":turn,"itemId":"reason-1","summaryIndex":0,"delta":"Checking the supplied directory."}}),
    );
    for (input, output) in [(15, 6), (30, 12)] {
        send(
            json!({"method":"thread/tokenUsage/updated","params":{"threadId":thread,"turnId":turn,"tokenUsage":{"total":{"totalTokens":input + output,"inputTokens":input,"outputTokens":output},"last":{"totalTokens":21}}}}),
        );
    }
    send(
        json!({"method":"fixture/futureNotification","params":{"threadId":thread,"turnId":turn,"detail":{"preserve":"all emitted fields"}}}),
    );
    // These are deliberately unrelated: no connected client should receive them.
    send(
        json!({"method":"fixture/futureNotification","params":{"threadId":"unrelated-native-thread","detail":"foreign-thread-marker"}}),
    );
    send(
        json!({"method":"account/rateLimits/updated","params":{"detail":"global-account-marker"}}),
    );
}

fn main() {
    let mut active_turns = HashMap::<String, String>::new();
    let mut questions = HashMap::<String, (String, String)>::new();
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("fixture-rpc.jsonl")
        .unwrap();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        writeln!(log, "{message}").unwrap();
        log.flush().unwrap();
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        if message.get("method").is_none() {
            if let Some((thread, turn)) = questions.remove(&id.to_string()) {
                let answer = message
                    .pointer("/result/answers/choice/answers/0")
                    .and_then(Value::as_str)
                    .unwrap_or("missing");
                send(
                    json!({"method":"item/completed","params":{"threadId":thread,"turnId":turn,"item":{"id":"question-answer","type":"agentMessage","text":format!("Answer: {answer}")}}}),
                );
                active_turns.remove(&thread);
                complete(&thread, &turn, "completed");
            }
            continue;
        }
        let method = message["method"].as_str().unwrap_or("");
        let params = &message["params"];
        let thread = params["threadId"].as_str().unwrap_or("");
        match method {
            "initialize" => send(
                json!({"id":id,"result":{"userAgent":"fake-codex","platformFamily":"unix","platformOs":"linux"}}),
            ),
            "account/read" => send(
                json!({"id":id,"result":{"account":{"type":"chatgpt","email":"fixture@example.invalid","planType":"plus"},"requiresOpenaiAuth":false}}),
            ),
            "account/rateLimits/read" => {
                if std::path::Path::new("rate-limits-unavailable").exists() {
                    send(
                        json!({"id":id,"error":{"code":-32000,"message":"private account detail: limits require ChatGPT authentication"}}),
                    );
                } else {
                    send(json!({"id":id,"result":{
                        "accountId":"private-account-id",
                        "ordinaryUsageAllowed":true,
                        "rateLimits":{"limitId":"codex","limitName":"Codex","planType":"plus","primary":{"usedPercent":10,"windowDurationMins":300,"resetsAt":1800000000},"secondary":{"usedPercent":20,"windowDurationMins":10080,"resetsAt":1800500000},"credits":{"balance":"private-credit-balance"}},
                        "rateLimitsByLimitId":{"codex-extra":{"limitId":"codex-extra","limitName":"Extra","normalModelSlug":"fixture-fast","primary":{"usedPercent":5,"windowDurationMins":10080,"resetsAt":1800500000}}},
                        "rateLimitResetCredits":{"credits":[{"id":"private-reset-credit"}]}
                    }}));
                }
            }
            "thread/start" => {
                if std::path::Path::new("exit-on-create").exists() {
                    return;
                }
                let tid = uuid::Uuid::new_v4().to_string();
                if std::path::Path::new("delay-create").exists() {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                }
                let model = params["model"].as_str().unwrap_or("fixture");
                let default_effort = if model == "fixture-fast" {
                    "low"
                } else {
                    "medium"
                };
                let effort = params["config"]["model_reasoning_effort"]
                    .as_str()
                    .unwrap_or(default_effort);
                // Native notifications may precede the thread/start response.
                send(
                    json!({"method":"thread/started","params":{"thread":{"id":tid,"cwd":params["cwd"]}}}),
                );
                send(
                    json!({"method":"thread/status/changed","params":{"threadId":tid,"status":{"type":"idle"},"marker":"creation-event"}}),
                );
                send(
                    json!({"method":"thread/status/changed","params":{"threadId":"unrelated-creation-thread","status":{"type":"idle"},"marker":"foreign-creation-event"}}),
                );
                send(
                    json!({"id":id,"result":{"thread":{"id":tid,"ephemeral":false},"model":model,"reasoningEffort":effort,"cwd":params["cwd"]}}),
                );
            }
            "turn/start" => {
                let prompt = params["input"][0]["text"]
                    .as_str()
                    .unwrap_or("")
                    .split("\n\n[Bridge handoff]\n")
                    .next()
                    .unwrap();
                let turn = uuid::Uuid::new_v4().to_string();
                active_turns.insert(thread.into(), turn.clone());
                let item = uuid::Uuid::new_v4().to_string();
                send(
                    json!({"id":id,"result":{"turn":{"id":turn,"status":"inProgress","items":[]}}}),
                );
                send(
                    json!({"method":"turn/started","params":{"threadId":thread,"turn":{"id":turn,"status":"inProgress"}}}),
                );
                if prompt == "wait until interrupted" {
                    continue;
                }
                if prompt == "exit during turn" {
                    return;
                }
                if prompt == "ask a question" {
                    let request = json!(format!("question-{turn}"));
                    questions.insert(request.to_string(), (thread.into(), turn.clone()));
                    send(
                        json!({"id":request,"method":"item/tool/requestUserInput","params":{"threadId":thread,"turnId":turn,"itemId":"question","questions":[{"id":"choice","header":"Target","question":"Which target?","options":[{"label":"Linux","description":"Linux target"},{"label":"Windows","description":"Windows target"}]}]}}),
                    );
                    continue;
                }
                if prompt == "read prepared task" {
                    let workspace = std::path::Path::new(params["cwd"].as_str().unwrap());
                    let task = std::fs::read_to_string(workspace.join("TASK.md")).unwrap();
                    std::fs::write(workspace.join("result.txt"), &task).unwrap();
                    send(
                        json!({"method":"item/completed","params":{"threadId":thread,"turnId":turn,"item":{"id":item,"type":"agentMessage","text":format!("Prepared task: {task}")}}}),
                    );
                    active_turns.remove(thread);
                    complete(thread, &turn, "completed");
                    continue;
                }
                if prompt == "show native output" {
                    native_output(thread, &turn);
                }
                if prompt == "noisy delegation" {
                    native_output(thread, &turn);
                    for index in 0..60 {
                        send(
                            json!({"method":"item/completed","params":{"threadId":thread,"turnId":turn,"item":{"id":format!("commentary-{index}"),"type":"agentMessage","phase":"commentary","text":format!("HUMAN-ONLY-PROGRESS {index}: {}", "checking the implementation; ".repeat(100))}}}),
                        );
                    }
                    send(
                        json!({"method":"item/completed","params":{"threadId":thread,"turnId":turn,"item":{"id":item,"type":"agentMessage","phase":"final_answer","text":"Done. Tests pass; see result.txt."}}}),
                    );
                    // A trailing commentary message must not replace the explicit final.
                    send(
                        json!({"method":"item/completed","params":{"threadId":thread,"turnId":turn,"item":{"id":"trailer","type":"agentMessage","phase":"commentary","text":"HUMAN-ONLY-TRAILER"}}}),
                    );
                    active_turns.remove(thread);
                    complete(thread, &turn, "completed");
                    continue;
                }
                if prompt == "long final answer" {
                    send(
                        json!({"method":"item/completed","params":{"threadId":thread,"turnId":turn,"item":{"id":item,"type":"agentMessage","phase":"final_answer","text":"résumé🙂\n".repeat(1500)}}}),
                    );
                    active_turns.remove(thread);
                    complete(thread, &turn, "completed");
                    continue;
                }
                if prompt == "show large output" {
                    send(
                        json!({"method":"item/commandExecution/outputDelta","params":{"threadId":thread,"turnId":turn,"itemId":"large-command","delta":"x".repeat(2 * 1024 * 1024)}}),
                    );
                }
                send(
                    json!({"method":"item/agentMessage/delta","params":{"threadId":thread,"turnId":turn,"itemId":item,"delta":"Hello "}}),
                );
                send(
                    json!({"method":"item/agentMessage/delta","params":{"threadId":thread,"turnId":turn,"itemId":item,"delta":"from Codex."}}),
                );
                send(
                    json!({"method":"item/completed","params":{"threadId":thread,"turnId":turn,"item":{"id":item,"type":"agentMessage","text":"Hello from Codex."}}}),
                );
                active_turns.remove(thread);
                complete(thread, &turn, "completed");
            }
            "turn/steer" => {
                let Some(turn) = active_turns.get(thread).cloned() else {
                    send(json!({"id":id,"error":{"code":-32600,"message":"No active turn"}}));
                    continue;
                };
                if params["expectedTurnId"] != turn {
                    send(
                        json!({"id":id,"error":{"code":-32600,"message":"Expected turn does not match active turn"}}),
                    );
                    continue;
                }
                let text = params["input"][0]["text"].as_str().unwrap_or("");
                if text == "complete while steering" {
                    active_turns.remove(thread);
                    complete(thread, &turn, "completed");
                    send(json!({"id":id,"error":{"code":-32600,"message":"No active turn"}}));
                } else if text == "reject steering" {
                    send(
                        json!({"id":id,"error":{"code":-32601,"message":"Steering is unavailable in this fixture mode"}}),
                    );
                } else {
                    send(json!({"id":id,"result":{"turnId":turn}}));
                    if text == "finish after steering" {
                        active_turns.remove(thread);
                        complete(thread, &turn, "completed");
                    }
                }
            }
            "turn/interrupt" => {
                active_turns.remove(thread);
                send(json!({"id":id,"result":{}}));
                send(
                    json!({"method":"turn/completed","params":{"threadId":thread,"turn":{"id":params["turnId"],"status":"interrupted","items":[]}}}),
                );
            }
            "thread/delete" => {
                if std::path::Path::new("fail-delete").exists() {
                    send(
                        json!({"id":id,"error":{"code":-32000,"message":"simulated delete failure"}}),
                    );
                } else {
                    active_turns.remove(thread);
                    send(json!({"id":id,"result":{}}));
                    send(json!({"method":"thread/deleted","params":{"threadId":thread}}));
                }
            }
            "thread/backgroundTerminals/clean" | "thread/name/set" | "thread/unsubscribe" => {
                send(json!({"id":id,"result":{}}))
            }
            "thread/read" => send(json!({"id":id,"result":{"thread":{"id":thread,"turns":[]}}})),
            "model/list" => {
                if std::path::Path::new("emit-usage-update").exists() {
                    send(
                        json!({"method":"account/rateLimits/updated","params":{"rateLimits":{"limitId":"codex","secondary":{"usedPercent":35},"primary":null,"planType":null},"accountId":"private-update-id"}}),
                    );
                }
                if params["cursor"] == "second-page" {
                    send(
                        json!({"id":id,"result":{"data":[{"id":"fixture-fast","model":"fixture-fast","displayName":"Fixture Fast","defaultReasoningEffort":"low","supportedReasoningEfforts":[{"reasoningEffort":"low","description":"Fast"},{"reasoningEffort":"medium","description":"Balanced"}],"isDefault":false}],"nextCursor":null}}),
                    );
                } else {
                    send(
                        json!({"id":id,"result":{"data":[{"id":"fixture","model":"fixture","displayName":"Fixture","defaultReasoningEffort":"medium","supportedReasoningEfforts":[{"reasoningEffort":"low","description":"Fast"},{"reasoningEffort":"medium","description":"Balanced"},{"reasoningEffort":"high","description":"Thorough"}],"isDefault":true}],"nextCursor":"second-page"}}),
                    );
                }
            }
            _ => send(
                json!({"id":id,"error":{"code":-32601,"message":format!("unknown fixture method: {method}")}}),
            ),
        }
    }
}
