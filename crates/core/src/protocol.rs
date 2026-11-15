use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 4;
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
// A native app-server line can contain up to 16 MiB of tool output. Keep
// complete events intact while retaining the smaller client-command limit.
pub const MAX_EVENT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionOptions {
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Exact server-side directory within the configured root. Blank allocates a new folder.
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub model: String,
    pub display_name: String,
    pub efforts: Vec<String>,
    pub default_effort: Option<String>,
    pub is_default: bool,
}

/// Sanitized account-wide allowance data. No identity, tokens or credit identifiers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountUsage {
    pub limits: Vec<RateLimitBucket>,
    pub ordinary_usage_allowed: Option<bool>,
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RateLimitBucket {
    pub id: String,
    pub name: Option<String>,
    pub model: Option<String>,
    pub plan: Option<String>,
    pub primary: Option<RateLimitWindow>,
    pub secondary: Option<RateLimitWindow>,
    pub spend_control_reached: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitWindow {
    pub used_percent: i32,
    pub window_minutes: Option<i64>,
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientCommand {
    NewSession {
        name: String,
        #[serde(default)]
        options: SessionOptions,
    },
    ListModels,
    Prompt {
        session_id: String,
        text: String,
    },
    Interrupt {
        session_id: String,
    },
    CloseSession {
        session_id: String,
    },
    Reply {
        session_id: String,
        request_id: String,
        result: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    AccountUsage {
        usage: AccountUsage,
    },
    Welcome {
        workspace: String,
        server_version: String,
    },
    Models {
        models: Vec<ModelInfo>,
        default_model: Option<String>,
    },
    SessionStarted {
        session_id: String,
        name: String,
        workspace: String,
        model: Option<String>,
        effort: Option<String>,
    },
    NativeEvent {
        session_id: String,
        method: String,
        params: Value,
    },
    PromptAccepted {
        session_id: String,
        steered: bool,
    },
    PromptRejected {
        session_id: String,
        text: String,
        message: String,
    },
    Delta {
        session_id: String,
        item_id: String,
        text: String,
    },
    ItemCompleted {
        session_id: String,
        item_id: String,
        text: String,
    },
    TurnStarted {
        session_id: String,
    },
    TurnFinished {
        session_id: String,
        status: String,
    },
    Activity {
        session_id: String,
        text: String,
    },
    Request {
        session_id: String,
        request_id: String,
        method: String,
        params: Value,
    },
    RequestResolved {
        session_id: String,
        request_id: String,
    },
    SessionClosed {
        session_id: String,
    },
    Error {
        session_id: Option<String>,
        message: String,
    },
}

impl ServerEvent {
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::Welcome { .. } | Self::Models { .. } | Self::AccountUsage { .. } => None,
            Self::Error { session_id, .. } => session_id.as_deref(),
            Self::SessionStarted { session_id, .. }
            | Self::NativeEvent { session_id, .. }
            | Self::PromptAccepted { session_id, .. }
            | Self::PromptRejected { session_id, .. }
            | Self::Delta { session_id, .. }
            | Self::ItemCompleted { session_id, .. }
            | Self::TurnStarted { session_id }
            | Self::TurnFinished { session_id, .. }
            | Self::Activity { session_id, .. }
            | Self::Request { session_id, .. }
            | Self::RequestResolved { session_id, .. }
            | Self::SessionClosed { session_id } => Some(session_id),
        }
    }
}
