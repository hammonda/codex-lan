use serde_json::Value;
use std::{fs, io, path::Path};

/// Read a snapshot while fake Codex may still be appending its next record.
pub fn calls(workspace: &Path) -> Vec<Value> {
    let path = workspace.join("fixture-rpc.jsonl");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => panic!("Could not read {}: {error}", path.display()),
    };
    bytes
        .split_inclusive(|byte| *byte == b'\n')
        // A write can stop anywhere, even in the middle of a UTF-8 character.
        // Only the terminating newline publishes a complete JSONL record.
        .take_while(|record| record.ends_with(b"\n"))
        .enumerate()
        .map(|(index, record)| {
            serde_json::from_slice(record).unwrap_or_else(|error| {
                panic!(
                    "Invalid complete fixture log record {} in {}: {error}",
                    index + 1,
                    path.display()
                )
            })
        })
        .collect()
}
