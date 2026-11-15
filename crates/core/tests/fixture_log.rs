#![cfg(feature = "test-support")]

mod support;

use serde_json::json;
use std::{fs::OpenOptions, io::Write};
use support::calls;

#[test]
fn snapshots_wait_for_complete_records_including_split_utf8() {
    let temp = tempfile::tempdir().unwrap();
    assert!(calls(temp.path()).is_empty());
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(temp.path().join("fixture-rpc.jsonl"))
        .unwrap();
    let first = json!({"method":"initialize"});
    write!(log, "{first}\r\n").unwrap();
    let next = json!({"method":"thread/start", "params":{"name":"hé"}});
    let encoded = serde_json::to_vec(&next).unwrap();
    let split = encoded.iter().position(|byte| *byte == 0xc3).unwrap() + 1;
    for chunk in [&encoded[..split], &encoded[split..]] {
        log.write_all(chunk).unwrap();
        log.flush().unwrap();
        assert_eq!(calls(temp.path()).as_slice(), std::slice::from_ref(&first));
    }
    log.write_all(b"\n").unwrap();
    log.flush().unwrap();
    assert_eq!(calls(temp.path()), [first, next]);
}

#[test]
#[should_panic(expected = "Invalid complete fixture log record 2")]
fn complete_malformed_records_are_not_silently_ignored() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("fixture-rpc.jsonl"),
        b"{\"method\":\"initialize\"}\n{\"broken\":}\n",
    )
    .unwrap();
    calls(temp.path());
}
