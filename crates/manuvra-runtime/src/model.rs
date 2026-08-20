use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SessionRole {
    Actor,
    Observer,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionMode {
    Background,
    Foreground,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    Active,
    Closing,
}

impl ExecutionMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Background => "background",
            Self::Foreground => "foreground",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetDescriptor {
    pub target_id: String,
    pub generation: u64,
    pub kind: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub target_id: String,
    pub target_generation: u64,
    pub role: SessionRole,
    pub mode: ExecutionMode,
    pub directory: PathBuf,
    pub lease_ttl_ms: u64,
    pub reference_namespace: String,
    pub reference_epoch: u64,
    pub frame_token: Option<String>,
    pub in_flight: usize,
    pub state: SessionState,
}

#[derive(Debug, Clone)]
pub struct ActorLease {
    pub session_id: String,
    pub target_generation: u64,
    pub ttl_ms: u64,
    pub expires_at: Instant,
    pub pinned: usize,
}

#[derive(Debug, Clone)]
pub struct AdapterContext {
    pub session_id: String,
    pub target_id: String,
    pub target_generation: u64,
    pub action_sequence: u64,
    pub reference_namespace: String,
    pub reference_epoch: u64,
    pub frame_token: Option<String>,
    pub mode: ExecutionMode,
    pub deadline: Instant,
}

#[derive(Debug, Clone)]
pub struct AdapterOperation {
    pub command: String,
    pub input: Value,
    pub prepared: Option<Value>,
}

impl AdapterOperation {
    pub fn new(command: String, input: Value) -> Self {
        Self {
            command,
            input,
            prepared: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterDelivery {
    Confirmed,
    Rejected,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct AdapterReply {
    pub delivery: AdapterDelivery,
    pub response: Value,
    pub screenshot: Option<Vec<u8>>,
    pub screenshot_width: Option<u32>,
    pub screenshot_height: Option<u32>,
    pub frame_signature: Option<String>,
    pub artifact: Option<AdapterArtifact>,
    pub error: Option<AdapterError>,
    pub timing: AdapterTiming,
    pub already_settled: bool,
    pub relevant_event_after_ms: Option<u64>,
    pub continuous_events: bool,
    pub capture_race_once: bool,
    pub interrupted: bool,
}

impl AdapterReply {
    pub fn confirmed(response: Value, screenshot: Option<Vec<u8>>) -> Self {
        Self {
            delivery: AdapterDelivery::Confirmed,
            response,
            screenshot,
            screenshot_width: None,
            screenshot_height: None,
            frame_signature: None,
            artifact: None,
            error: None,
            timing: AdapterTiming::default(),
            already_settled: false,
            relevant_event_after_ms: None,
            continuous_events: false,
            capture_race_once: false,
            interrupted: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct AdapterTiming {
    pub preflight_ms: u64,
    pub dispatch_ms: u64,
    pub stabilize_ms: u64,
    pub capture_ms: u64,
}

#[derive(Debug, Clone)]
pub struct AdapterArtifact {
    pub kind: String,
    pub extension: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct AdapterError {
    pub code: String,
    pub message: Option<String>,
    pub details: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct AdapterSession {
    pub session_id: String,
    pub target_id: String,
    pub target_generation: u64,
}

pub trait TargetAdapter: Send + Sync {
    fn targets(&self) -> Vec<TargetDescriptor>;
    fn targets_until(&self, deadline: Instant) -> Result<Vec<TargetDescriptor>, AdapterError> {
        if Instant::now() >= deadline {
            return Err(AdapterError {
                code: "timed_out".to_owned(),
                message: Some("deadline elapsed before target discovery".to_owned()),
                details: None,
            });
        }
        let targets = self.targets();
        if Instant::now() >= deadline {
            return Err(AdapterError {
                code: "timed_out".to_owned(),
                message: Some("deadline elapsed during target discovery".to_owned()),
                details: None,
            });
        }
        Ok(targets)
    }
    fn diagnostics(&self) -> Value {
        Value::Null
    }
    /// Requests host permissions owned by this adapter, if it has any.
    ///
    /// The default keeps adapters without host privacy permissions passive. Implementations must
    /// preflight first, request only missing permissions before `deadline`, and return freshly
    /// rechecked facts.
    fn setup_permissions(&self, _deadline: Instant) -> Option<Result<Value, AdapterError>> {
        None
    }
    fn session_opened(
        &self,
        _session: &AdapterSession,
        deadline: Instant,
    ) -> Result<(), AdapterError> {
        if Instant::now() >= deadline {
            return Err(AdapterError {
                code: "timed_out".to_owned(),
                message: Some("deadline elapsed before adapter session initialization".to_owned()),
                details: None,
            });
        }
        Ok(())
    }
    fn session_closed(&self, _session: &AdapterSession) {}
    fn prepare(
        &self,
        _context: &AdapterContext,
        operation: &AdapterOperation,
        _cancellation: Arc<AtomicBool>,
    ) -> Result<AdapterOperation, AdapterError> {
        Ok(operation.clone())
    }
    fn invoke(
        &self,
        context: &AdapterContext,
        operation: &AdapterOperation,
        cancellation: Arc<AtomicBool>,
    ) -> AdapterReply;
}
