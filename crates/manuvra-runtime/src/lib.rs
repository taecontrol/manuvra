mod actions;
mod artifacts;
mod commands;
pub mod fake;
#[cfg(debug_assertions)]
pub mod fake_diagnostics;
mod model;
mod observations;
mod usage;
mod util;
mod validation;

pub use model::{
    AdapterArtifact, AdapterContext, AdapterDelivery, AdapterError, AdapterOperation, AdapterReply,
    AdapterSession, AdapterTiming, ExecutionMode, TargetAdapter, TargetDescriptor,
};

use artifacts::ArtifactStore;
use manuvra_protocol::{
    CommandId, Invocation, build_digest, canonical_invocation_digest, command_descriptor,
    encode_operational_line, operational_error,
};
use model::{ActorLease, Session};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Mutex as AsyncMutex;
use usage::UsageStore;
use util::opaque_id;

pub(crate) type LocatedTarget = (TargetDescriptor, Arc<dyn TargetAdapter>);

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub temporary_root: PathBuf,
    pub config_root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct InvocationReply {
    pub value: Value,
    pub exit_code: i32,
}

impl InvocationReply {
    pub fn success(value: Value) -> Self {
        Self {
            value,
            exit_code: 0,
        }
    }

    pub fn error(code: &str, message: Option<&str>) -> Self {
        let (error, exit_code) = operational_error(code, message);
        Self {
            value: json!({"error": error}),
            exit_code,
        }
    }

    fn enforce_bound(self) -> Self {
        if encode_operational_line(&self.value).is_ok() {
            return self;
        }
        Self::error("internal_result_overflow", None)
    }
}

pub trait InteractionModule: Send + Sync {
    fn invoke(&self, invocation: Invocation) -> InvocationReply;
}

pub struct Runtime {
    pub(crate) state: Mutex<RuntimeState>,
    request_completion: Condvar,
    pub(crate) target_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    pub(crate) adapters: Vec<Arc<dyn TargetAdapter>>,
    pub(crate) artifacts: ArtifactStore,
    pub(crate) usage: UsageStore,
    pub(crate) daemon_instance: String,
    pub(crate) startup_removed: Vec<PathBuf>,
    pub(crate) startup_preserved: Vec<PathBuf>,
    pub(crate) setup_installation: Value,
    pub(crate) doctor_warnings: Vec<String>,
}

pub(crate) struct RuntimeState {
    pub sessions: HashMap<String, Session>,
    pub leases: HashMap<String, ActorLease>,
    pub action_sequences: HashMap<String, u64>,
    pub terminal_requests: HashMap<String, CachedReply>,
    pub cancellations: HashMap<String, Arc<AtomicBool>>,
    pub cancellation_sessions: HashMap<String, String>,
    pub pending_requests: HashMap<String, String>,
}

#[derive(Clone)]
pub(crate) struct CachedReply {
    pub digest: String,
    pub reply: InvocationReply,
}

enum RequestClaim {
    Owner,
    Ready(InvocationReply),
}

impl Runtime {
    pub fn new(
        config: RuntimeConfig,
        adapters: Vec<Arc<dyn TargetAdapter>>,
    ) -> Result<Self, String> {
        let daemon_instance = opaque_id("d_");
        let artifacts = ArtifactStore::new(&config.temporary_root, daemon_instance.clone())
            .map_err(|error| error.to_string())?;
        let cleanup = artifacts
            .cleanup_orphans()
            .map_err(|error| error.to_string())?;
        let usage = UsageStore::new(config.config_root).map_err(|error| error.to_string())?;
        Ok(Self {
            state: Mutex::new(RuntimeState {
                sessions: HashMap::new(),
                leases: HashMap::new(),
                action_sequences: HashMap::new(),
                terminal_requests: HashMap::new(),
                cancellations: HashMap::new(),
                cancellation_sessions: HashMap::new(),
                pending_requests: HashMap::new(),
            }),
            request_completion: Condvar::new(),
            target_locks: Mutex::new(HashMap::new()),
            adapters,
            artifacts,
            usage,
            daemon_instance,
            startup_removed: cleanup.removed,
            startup_preserved: cleanup.preserved,
            setup_installation: json!({}),
            doctor_warnings: Vec::new(),
        })
    }

    /// Binds daemon installation identity to setup replies before request ownership is claimed.
    pub fn with_setup_installation(mut self, installation: Value) -> Self {
        self.setup_installation = installation;
        self
    }

    /// Binds deterministic debug-only doctor warnings before request ownership is claimed.
    #[cfg(debug_assertions)]
    pub fn with_doctor_warnings(mut self, warnings: Vec<String>) -> Self {
        self.doctor_warnings = warnings;
        self
    }

    pub fn targets(&self) -> Vec<TargetDescriptor> {
        self.adapters
            .iter()
            .flat_map(|adapter| adapter.targets())
            .collect()
    }

    pub fn lifecycle_snapshot(&self) -> Value {
        let state = self.state.lock().expect("runtime state");
        let mut sessions = state
            .sessions
            .values()
            .map(|session| {
                json!({
                    "session_id": session.id,
                    "target_id": session.target_id,
                    "role": session.role,
                    "mode": session.mode,
                })
            })
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| {
            left["session_id"]
                .as_str()
                .cmp(&right["session_id"].as_str())
        });
        json!({
            "active_sessions": sessions,
            "pending_requests": state.pending_requests.len(),
        })
    }

    pub fn has_active_sessions(&self) -> bool {
        !self
            .state
            .lock()
            .expect("runtime state")
            .sessions
            .is_empty()
    }

    pub(crate) fn targets_until(
        &self,
        deadline: Instant,
    ) -> Result<Vec<TargetDescriptor>, crate::model::AdapterError> {
        let mut targets = Vec::new();
        for adapter in &self.adapters {
            targets.extend(adapter.targets_until(deadline)?);
        }
        Ok(targets)
    }

    pub(crate) fn target_until(
        &self,
        target_id: &str,
        deadline: Instant,
    ) -> Result<Option<TargetDescriptor>, crate::model::AdapterError> {
        Ok(self
            .targets_until(deadline)?
            .into_iter()
            .find(|target| target.target_id == target_id))
    }

    pub(crate) fn target_with_adapter_until(
        &self,
        target_id: &str,
        deadline: Instant,
    ) -> Result<Option<LocatedTarget>, crate::model::AdapterError> {
        for adapter in &self.adapters {
            if let Some(target) = adapter
                .targets_until(deadline)?
                .into_iter()
                .find(|target| target.target_id == target_id)
            {
                return Ok(Some((target, adapter.clone())));
            }
        }
        Ok(None)
    }

    pub(crate) fn adapter_for_until(
        &self,
        target_id: &str,
        deadline: Instant,
    ) -> Result<Option<Arc<dyn TargetAdapter>>, crate::model::AdapterError> {
        Ok(self
            .target_with_adapter_until(target_id, deadline)?
            .map(|(_, adapter)| adapter))
    }

    pub(crate) fn target_lock(&self, target_id: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self.target_locks.lock().expect("target lock table");
        locks
            .entry(target_id.to_owned())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    fn validate_envelope(&self, invocation: &Invocation) -> Result<(), InvocationReply> {
        validate_protocol_identity(invocation)?;
        validate_request_identity(invocation)?;
        Ok(())
    }

    fn claim_request(
        &self,
        invocation: &Invocation,
        started: Instant,
    ) -> Result<RequestClaim, InvocationReply> {
        let digest = canonical_invocation_digest(invocation);
        let deadline = started + Duration::from_millis(invocation.deadline_ms);
        let mut state = self.state.lock().expect("runtime state");
        loop {
            match take_request_claim(&mut state, invocation, &digest) {
                Some(claim) => return claim,
                None => state = self.await_pending_owner(state, deadline)?,
            }
        }
    }

    fn await_pending_owner<'a>(
        &'a self,
        state: std::sync::MutexGuard<'a, RuntimeState>,
        deadline: Instant,
    ) -> Result<std::sync::MutexGuard<'a, RuntimeState>, InvocationReply> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(InvocationReply::error("timed_out", None));
        }
        let waited = self
            .request_completion
            .wait_timeout(state, remaining)
            .expect("runtime state");
        if waited.1.timed_out() {
            return Err(InvocationReply::error("timed_out", None));
        }
        Ok(waited.0)
    }

    fn finish_request(&self, invocation: &Invocation, reply: InvocationReply) -> InvocationReply {
        let mut state = self.state.lock().expect("runtime state");
        state.pending_requests.remove(&invocation.request_id);
        state.terminal_requests.insert(
            invocation.request_id.clone(),
            CachedReply {
                digest: canonical_invocation_digest(invocation),
                reply: reply.clone(),
            },
        );
        self.request_completion.notify_all();
        reply
    }

    fn dispatch(&self, invocation: &Invocation, started: Instant) -> InvocationReply {
        match CommandId::parse(&invocation.command).expect("validated command ID") {
            CommandId::SystemCommandsList
            | CommandId::SystemCommandsGet
            | CommandId::SystemCommandsSchema
            | CommandId::SystemCommandsErrors => self.discovery(invocation, started),
            CommandId::SystemCommandsUsage => self.usage_command(invocation, started),
            CommandId::ObserveQuery
            | CommandId::ObserveScreenshot
            | CommandId::ObserveTree
            | CommandId::ObserveEvidence => self.observe(invocation, started),
            CommandId::ActionClick
            | CommandId::ActionType
            | CommandId::ActionPress
            | CommandId::ActionScroll
            | CommandId::ActionNavigate
            | CommandId::RawCdp
            | CommandId::RawAxGet
            | CommandId::RawAxSet
            | CommandId::RawAxPerform => self.execute_adapter_command(invocation, started),
            other => self.dispatch_control(invocation, started, other),
        }
    }

    fn dispatch_control(
        &self,
        invocation: &Invocation,
        started: Instant,
        command: CommandId,
    ) -> InvocationReply {
        match command {
            CommandId::TargetList
            | CommandId::SessionOpen
            | CommandId::SessionClose
            | CommandId::LeaseManage => self.dispatch_session(invocation, started, command),
            CommandId::RequestCancel
            | CommandId::ArtifactExport
            | CommandId::SystemDoctor
            | CommandId::SystemSetup => self.dispatch_support(invocation, started, command),
            CommandId::DaemonStatus
            | CommandId::DaemonStop
            | CommandId::SystemMigrate
            | CommandId::SystemPurge
            | CommandId::SystemChromeLaunch => InvocationReply::error("command_unsupported", None),
            _ => unreachable!("command family already dispatched"),
        }
    }

    fn dispatch_session(
        &self,
        invocation: &Invocation,
        started: Instant,
        command: CommandId,
    ) -> InvocationReply {
        match command {
            CommandId::TargetList => self.list_targets(invocation, started),
            CommandId::SessionOpen => self.open_session(invocation, started),
            CommandId::SessionClose => self.close_session(invocation, started),
            CommandId::LeaseManage => self.manage_lease(invocation, started),
            _ => unreachable!("session family already selected"),
        }
    }

    fn dispatch_support(
        &self,
        invocation: &Invocation,
        started: Instant,
        command: CommandId,
    ) -> InvocationReply {
        match command {
            CommandId::RequestCancel => self.cancel(invocation, started),
            CommandId::ArtifactExport => self.export(invocation, started),
            CommandId::SystemDoctor => self.doctor(invocation, started),
            CommandId::SystemSetup => self.setup(invocation, started),
            _ => unreachable!("support family already selected"),
        }
    }
}

fn take_request_claim(
    state: &mut RuntimeState,
    invocation: &Invocation,
    digest: &str,
) -> Option<Result<RequestClaim, InvocationReply>> {
    if let Some(cached) = state.terminal_requests.get(&invocation.request_id) {
        return Some(Ok(RequestClaim::Ready(replay_or_conflict(cached, digest))));
    }
    match state.pending_requests.get(&invocation.request_id) {
        Some(pending) if pending != digest => {
            Some(Err(InvocationReply::error("request_id_conflict", None)))
        }
        Some(_) => None,
        None => {
            state
                .pending_requests
                .insert(invocation.request_id.clone(), digest.to_owned());
            Some(Ok(RequestClaim::Owner))
        }
    }
}

fn replay_or_conflict(cached: &CachedReply, digest: &str) -> InvocationReply {
    if cached.digest == digest {
        cached.reply.clone()
    } else {
        InvocationReply::error("request_id_conflict", None)
    }
}

fn enforce_result_schema(invocation: &Invocation, reply: InvocationReply) -> InvocationReply {
    if reply.exit_code != 0
        && reply.value.get("schema").and_then(Value::as_str) != Some("manuvra/action-result@1")
    {
        return reply;
    }
    match manuvra_protocol::validate_command_result(&invocation.command, &reply.value) {
        Ok(()) => reply,
        Err(message) => InvocationReply::error("internal_error", Some(&message)),
    }
}

fn validate_protocol_identity(invocation: &Invocation) -> Result<(), InvocationReply> {
    if invocation.protocol.major != 1 || invocation.protocol.minimum_minor > 0 {
        return Err(InvocationReply::error("incompatible_protocol", None));
    }
    if invocation.registry_version != manuvra_protocol::REGISTRY_VERSION {
        return Err(InvocationReply::error("unsupported_command_version", None));
    }
    if invocation.build_digest != build_digest() {
        return Err(InvocationReply::error("daemon_version_mismatch", None));
    }
    Ok(())
}

fn validate_request_identity(invocation: &Invocation) -> Result<(), InvocationReply> {
    validate_deadline_ms(invocation)?;
    validate_request_id(invocation)?;
    validate_known_command(invocation)
}

fn validate_deadline_ms(invocation: &Invocation) -> Result<(), InvocationReply> {
    if (50..=120_000).contains(&invocation.deadline_ms) {
        Ok(())
    } else {
        Err(InvocationReply::error(
            "invalid_request",
            Some("deadline_ms must be between 50 and 120000"),
        ))
    }
}

fn validate_request_id(invocation: &Invocation) -> Result<(), InvocationReply> {
    if invocation.request_id.is_empty() || invocation.request_id.len() > 128 {
        Err(InvocationReply::error(
            "invalid_request",
            Some("request_id must contain 1 through 128 bytes"),
        ))
    } else {
        Ok(())
    }
}

fn validate_known_command(invocation: &Invocation) -> Result<(), InvocationReply> {
    if command_descriptor(&invocation.command).is_none() {
        return Err(InvocationReply::error("unknown_command", None));
    }
    manuvra_protocol::validate_command_input(&invocation.command, &invocation.input)
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))
}

impl InteractionModule for Runtime {
    fn invoke(&self, invocation: Invocation) -> InvocationReply {
        if let Err(reply) = self.validate_envelope(&invocation) {
            return reply.enforce_bound();
        }
        let started = Instant::now();
        match self.claim_request(&invocation, started) {
            Ok(RequestClaim::Owner) => {}
            Ok(RequestClaim::Ready(reply)) | Err(reply) => return reply.enforce_bound(),
        }
        let dispatched = catch_unwind(AssertUnwindSafe(|| self.dispatch(&invocation, started)))
            .unwrap_or_else(|_| InvocationReply::error("internal_error", None));
        let reply = enforce_result_schema(&invocation, dispatched).enforce_bound();
        self.finish_request(&invocation, reply)
    }
}
