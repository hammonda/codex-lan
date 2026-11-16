use super::*;

impl SessionObserver {
    /// Deterministic sample traffic for inspecting the layout without model calls.
    pub(crate) fn demo_traffic(&mut self) {
        self.client = "Claude";
        let id = self.session.id.clone();
        self.command(ClientCommand::Prompt { session_id: id.clone(), text: "Read TASK.md in the prepared directory. Summarize the supplied source files, write SUMMARY.md, then report the result and any limitations.".into() });
        self.event(ServerEvent::ItemCompleted {
            session_id: id.clone(),
            item_id: "intro".into(),
            text: "I’ll read the task and check the prepared files first.".into(),
        });
        self.event(ServerEvent::NativeEvent { session_id: id.clone(), method: "item/started".into(), params: json!({"item":{"id":"command","type":"commandExecution","command":format!("python - <<'PY'\n{}\nPY", "print('checking prepared context')\n".repeat(40))}}) });
        self.event(ServerEvent::Activity {
            session_id: id.clone(),
            text: format!(
                "Finished commandExecution python - <<'PY'\n{}",
                "print('checking prepared context')\n".repeat(40)
            ),
        });
        self.event(ServerEvent::NativeEvent {
            session_id: id.clone(),
            method: "item/commandExecution/outputDelta".into(),
            params: json!({"itemId":"command","delta":"TASK.md\nsource-notes.md\nREADME.md\n"}),
        });
        self.command(ClientCommand::Prompt {
            session_id: id.clone(),
            text: "Keep the summary under 300 words and include a short list of open questions."
                .into(),
        });
        self.event(ServerEvent::PromptAccepted {
            session_id: id.clone(),
            steered: true,
            turn_id: "turn".into(),
        });
        self.event(ServerEvent::ItemCompleted {
            session_id: id.clone(),
            item_id: "steering".into(),
            text:
                "Understood. I’ll keep it concise and separate confirmed facts from open questions."
                    .into(),
        });
        self.event(ServerEvent::NativeEvent { session_id: id.clone(), method: "item/completed".into(), params: json!({"item":{"id":"command","type":"commandExecution","exitCode":0,"aggregatedOutput":"TASK.md\nsource-notes.md\nREADME.md\n"}}) });
        self.event(ServerEvent::ItemCompleted { session_id: id.clone(), item_id: "final".into(), text: "Saved SUMMARY.md in the prepared directory. It contains a 240-word summary and three open questions. All statements were checked against the supplied files.".into() });
        self.event(ServerEvent::TurnFinished {
            session_id: id,
            turn_id: "turn".into(),
            status: "completed".into(),
        });
        self.session.output_level = match std::env::var("CODEX_LAN_DEMO_OUTPUT").as_deref() {
            Ok("replies") => OutputLevel::Replies,
            Ok("everything") => OutputLevel::Everything,
            Ok("tools") => OutputLevel::Tools,
            _ => OutputLevel::Progress,
        };
    }
}
