use crate::artifacts::ArtifactError;
use crate::model::{
    ActorLease, AdapterSession, ExecutionMode, Session, SessionRole, SessionState, TargetAdapter,
    TargetDescriptor,
};
use crate::usage::UsageError;
use crate::util::{opaque_id, rfc3339_after};
use crate::validation::Input;
use crate::{InvocationReply, LocatedTarget, Runtime, RuntimeState};
use manuvra_protocol::{
    Invocation, command_descriptor, command_help, error_meta, registry_page, schema_pointer,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

struct OpenRequest {
    target_id: String,
    role: SessionRole,
    mode: ExecutionMode,
    lease_ttl_ms: u64,
}

struct ExportRequest {
    session_id: String,
    destination: PathBuf,
    selected: Option<Vec<String>>,
}

struct TargetListFilter<'a> {
    kind: Option<&'a str>,
    cursor: usize,
    limit: usize,
}

struct LeaseRequest<'a> {
    session_id: &'a str,
    action: &'a str,
    requested_ttl: u64,
}

struct OpenedAttach {
    request: OpenRequest,
    adapter: Arc<dyn TargetAdapter>,
    adapter_session: AdapterSession,
    deadline: Instant,
}

impl Runtime {
    pub(crate) fn discovery(&self, invocation: &Invocation, started: Instant) -> InvocationReply {
        if deadline_expired(invocation, started) {
            return InvocationReply::error("timed_out", None);
        }
        let reply = match invocation.command.as_str() {
            "system.commands.list" => discovery_list(&invocation.input),
            "system.commands.get" => discovery_get(&invocation.input),
            "system.commands.schema" => discovery_schema(&invocation.input),
            "system.commands.errors" => discovery_error(&invocation.input),
            _ => InvocationReply::error("unknown_command", None),
        };
        enforce_deadline(invocation, started, reply)
    }

    pub(crate) fn usage_command(
        &self,
        invocation: &Invocation,
        started: Instant,
    ) -> InvocationReply {
        if deadline_expired(invocation, started) {
            return InvocationReply::error("timed_out", None);
        }
        let (action, destination) = match parse_usage_request(invocation) {
            Ok(request) => request,
            Err(reply) => return reply,
        };
        let reply = match self.usage.manage(action, destination) {
            Ok(result) => {
                InvocationReply::success(serde_json::to_value(result).expect("usage result"))
            }
            Err(error) => usage_error_reply(error),
        };
        enforce_deadline(invocation, started, reply)
    }

    pub(crate) fn list_targets(
        &self,
        invocation: &Invocation,
        started: Instant,
    ) -> InvocationReply {
        if deadline_expired(invocation, started) {
            return InvocationReply::error("timed_out", None);
        }
        let filter = match parse_target_list_request(invocation) {
            Ok(filter) => filter,
            Err(reply) => return reply,
        };
        let all = match self.targets_until(started + Duration::from_millis(invocation.deadline_ms))
        {
            Ok(targets) => targets,
            Err(error) => return InvocationReply::error(&error.code, error.message.as_deref()),
        }
        .into_iter()
        .filter(|target| filter.kind.is_none_or(|expected| target.kind == expected))
        .collect::<Vec<_>>();
        let end = (filter.cursor + filter.limit).min(all.len());
        let state = self.state.lock().expect("runtime state");
        let targets = all[filter.cursor.min(all.len())..end]
            .iter()
            .map(|target| target_list_entry(&state, target))
            .collect::<Vec<_>>();
        enforce_deadline(
            invocation,
            started,
            InvocationReply::success(json!({
                "targets": targets,
                "next_cursor": (end < all.len()).then(|| end.to_string()),
            })),
        )
    }

    pub(crate) fn open_session(
        &self,
        invocation: &Invocation,
        started: Instant,
    ) -> InvocationReply {
        if deadline_expired(invocation, started) {
            return InvocationReply::error("timed_out", None);
        }
        let request = match parse_open_request(invocation) {
            Ok(request) => request,
            Err(reply) => return reply,
        };
        let (target, adapter) =
            match self.locate_open_target(&request.target_id, invocation, started) {
                Ok(located) => located,
                Err(reply) => return reply,
            };
        self.complete_open_session(invocation, started, request, target, adapter)
    }

    fn locate_open_target(
        &self,
        target_id: &str,
        invocation: &Invocation,
        started: Instant,
    ) -> Result<LocatedTarget, InvocationReply> {
        match self.target_with_adapter_until(
            target_id,
            started + Duration::from_millis(invocation.deadline_ms),
        ) {
            Ok(Some(target)) => Ok(target),
            Err(error) => Err(InvocationReply::error(
                &error.code,
                error.message.as_deref(),
            )),
            Ok(None) => Err(InvocationReply::error("target_not_found", None)),
        }
    }

    fn complete_open_session(
        &self,
        invocation: &Invocation,
        started: Instant,
        request: OpenRequest,
        target: TargetDescriptor,
        adapter: Arc<dyn TargetAdapter>,
    ) -> InvocationReply {
        let deadline = started + Duration::from_millis(invocation.deadline_ms);
        let session_id = opaque_id("s_");
        let directory =
            match self
                .artifacts
                .create_session(&session_id, &request.target_id, target.generation)
            {
                Ok(directory) => directory,
                Err(error) => {
                    return InvocationReply::error("artifact_io_failed", Some(&error.to_string()));
                }
            };
        if deadline_expired(invocation, started) {
            let _ = self.artifacts.close_session(&directory);
            return InvocationReply::error("timed_out", None);
        }
        let session = new_open_session(&session_id, &request, &target, directory.clone());
        if let Err(reply) = self.publish_session(session) {
            let _ = self.artifacts.close_session(&directory);
            return reply;
        }
        let adapter_session = AdapterSession {
            session_id: session_id.clone(),
            target_id: request.target_id.clone(),
            target_generation: target.generation,
        };
        self.attach_opened_session(
            invocation,
            started,
            OpenedAttach {
                request,
                adapter,
                adapter_session,
                deadline,
            },
        )
    }

    fn attach_opened_session(
        &self,
        invocation: &Invocation,
        started: Instant,
        opened: OpenedAttach,
    ) -> InvocationReply {
        if let Err(error) = opened
            .adapter
            .session_opened(&opened.adapter_session, opened.deadline)
        {
            self.rollback_open_session(
                &opened.adapter_session.session_id,
                &opened.adapter,
                &opened.adapter_session,
            );
            return InvocationReply::error(&error.code, error.message.as_deref());
        }
        if deadline_expired(invocation, started) {
            self.rollback_open_session(
                &opened.adapter_session.session_id,
                &opened.adapter,
                &opened.adapter_session,
            );
            return InvocationReply::error("timed_out", None);
        }
        let lease = (opened.request.role == SessionRole::Actor).then(|| {
            lease_json(
                &opened.adapter_session.session_id,
                opened.request.lease_ttl_ms,
                "held",
            )
        });
        InvocationReply::success(json!({
            "session_id": opened.adapter_session.session_id,
            "target_id": opened.request.target_id,
            "target_generation": opened.adapter_session.target_generation,
            "role": opened.request.role,
            "mode": opened.request.mode,
            "state": "active",
            "lease": lease,
        }))
    }

    fn rollback_open_session(
        &self,
        session_id: &str,
        adapter: &Arc<dyn crate::model::TargetAdapter>,
        adapter_session: &AdapterSession,
    ) {
        let session = {
            let mut state = self.state.lock().expect("runtime state");
            let session = state.sessions.remove(session_id);
            if let Some(session) = &session
                && state
                    .leases
                    .get(&session.target_id)
                    .is_some_and(|lease| lease.session_id == session_id)
            {
                state.leases.remove(&session.target_id);
            }
            session
        };
        if let Some(session) = session {
            let _ = self.artifacts.close_session(&session.directory);
        }
        adapter.session_closed(adapter_session);
    }

    fn publish_session(&self, session: Session) -> Result<(), InvocationReply> {
        let now = Instant::now();
        let mut state = self.state.lock().expect("runtime state");
        expire_target_lease(&mut state.leases, &session.target_id, now);
        if state
            .leases
            .get(&session.target_id)
            .is_some_and(|lease| lease.target_generation != session.target_generation)
        {
            state.leases.remove(&session.target_id);
        }
        if session.role == SessionRole::Actor && state.leases.contains_key(&session.target_id) {
            return Err(InvocationReply::error("actor_lease_held", None));
        }
        if session.role == SessionRole::Actor {
            state.leases.insert(
                session.target_id.clone(),
                ActorLease {
                    session_id: session.id.clone(),
                    target_generation: session.target_generation,
                    ttl_ms: session.lease_ttl_ms,
                    expires_at: now + Duration::from_millis(session.lease_ttl_ms),
                    pinned: 0,
                },
            );
        }
        state
            .action_sequences
            .entry(session.target_id.clone())
            .or_insert(0);
        state.sessions.insert(session.id.clone(), session);
        Ok(())
    }

    pub(crate) fn close_session(
        &self,
        invocation: &Invocation,
        started: Instant,
    ) -> InvocationReply {
        let (session_id, cancel_running) = match parse_close_request(invocation) {
            Ok(request) => request,
            Err(reply) => return reply,
        };
        if let Err(reply) = self.begin_close(session_id, cancel_running) {
            return reply;
        }
        let deadline = started + Duration::from_millis(invocation.deadline_ms);
        if cancel_running && !self.await_session_idle(session_id, deadline) {
            self.clear_closing(session_id);
            return InvocationReply::error(
                "timed_out",
                Some("session work did not become terminal before close deadline"),
            );
        }
        self.finish_close(session_id, deadline)
    }

    fn begin_close(&self, session_id: &str, cancel_running: bool) -> Result<(), InvocationReply> {
        let mut state = self.state.lock().expect("runtime state");
        let in_flight = closing_in_flight(&state, session_id)?;
        if in_flight > 0 && !cancel_running {
            return Err(InvocationReply::error("session_busy", None));
        }
        state
            .sessions
            .get_mut(session_id)
            .expect("checked session")
            .state = SessionState::Closing;
        if cancel_running {
            cancel_session_requests(&state, session_id);
        }
        Ok(())
    }

    fn await_session_idle(&self, session_id: &str, deadline: Instant) -> bool {
        while Instant::now() < deadline {
            let idle = self
                .state
                .lock()
                .expect("runtime state")
                .sessions
                .get(session_id)
                .is_some_and(|session| session.in_flight == 0);
            if idle {
                return true;
            }
            thread::sleep(Duration::from_millis(2));
        }
        false
    }

    fn clear_closing(&self, session_id: &str) {
        if let Some(session) = self
            .state
            .lock()
            .expect("runtime state")
            .sessions
            .get_mut(session_id)
        {
            session.state = SessionState::Active;
        }
    }

    fn finish_close(&self, session_id: &str, deadline: Instant) -> InvocationReply {
        let session =
            match idle_closing_session(&self.state.lock().expect("runtime state"), session_id) {
                Ok(session) => session,
                Err(reply) => return reply,
            };
        if let Err(reply) = self.release_closing_resources(&session, session_id, deadline) {
            return reply;
        }
        forget_closed_session(
            &mut self.state.lock().expect("runtime state"),
            session_id,
            &session.target_id,
        );
        InvocationReply::success(json!({
            "session_id": session_id,
            "closed": true,
            "artifacts_removed": true,
        }))
    }

    fn release_closing_resources(
        &self,
        session: &Session,
        session_id: &str,
        deadline: Instant,
    ) -> Result<(), InvocationReply> {
        let adapter = match self.adapter_for_until(&session.target_id, deadline) {
            Ok(adapter) => adapter,
            Err(error) => {
                self.clear_closing(session_id);
                return Err(InvocationReply::error(
                    &error.code,
                    error.message.as_deref(),
                ));
            }
        };
        if let Err(error) = self.artifacts.close_session(&session.directory) {
            self.clear_closing(session_id);
            return Err(InvocationReply::error(
                "artifact_io_failed",
                Some(&error.to_string()),
            ));
        }
        if let Some(adapter) = adapter {
            adapter.session_closed(&AdapterSession {
                session_id: session.id.clone(),
                target_id: session.target_id.clone(),
                target_generation: session.target_generation,
            });
        }
        Ok(())
    }

    pub(crate) fn manage_lease(
        &self,
        invocation: &Invocation,
        started: Instant,
    ) -> InvocationReply {
        if deadline_expired(invocation, started) {
            return InvocationReply::error("timed_out", None);
        }
        let request = match parse_lease_request(invocation) {
            Ok(request) => request,
            Err(reply) => return reply,
        };
        enforce_deadline(
            invocation,
            started,
            self.apply_lease_action(
                request.session_id,
                request.action,
                request.requested_ttl,
                started + Duration::from_millis(invocation.deadline_ms),
            ),
        )
    }

    fn apply_lease_action(
        &self,
        session_id: &str,
        action: &str,
        requested_ttl: u64,
        deadline: Instant,
    ) -> InvocationReply {
        self.apply_lease_action_checked(session_id, action, requested_ttl, deadline)
            .unwrap_or_else(|reply| reply)
    }

    fn apply_lease_action_checked(
        &self,
        session_id: &str,
        action: &str,
        requested_ttl: u64,
        deadline: Instant,
    ) -> Result<InvocationReply, InvocationReply> {
        let now = Instant::now();
        let mut state = self.state.lock().expect("runtime state");
        let session = actor_session(&state, session_id)?;
        self.validate_session_generation(&session, deadline)?;
        expire_target_lease(&mut state.leases, &session.target_id, now);
        let ttl = validated_lease_ttl(requested_ttl, session.lease_ttl_ms)?;
        Ok(apply_validated_lease_action(
            &mut state.leases,
            &session,
            action,
            ttl,
            now,
        ))
    }

    fn validate_session_generation(
        &self,
        session: &Session,
        deadline: Instant,
    ) -> Result<(), InvocationReply> {
        match self
            .target_until(&session.target_id, deadline)
            .map_err(|error| InvocationReply::error(&error.code, error.message.as_deref()))?
        {
            Some(target) if target.generation == session.target_generation => Ok(()),
            Some(_) => Err(InvocationReply::error("target_stale", None)),
            None => Err(InvocationReply::error("target_not_found", None)),
        }
    }

    pub(crate) fn cancel(&self, invocation: &Invocation, started: Instant) -> InvocationReply {
        if deadline_expired(invocation, started) {
            return InvocationReply::error("timed_out", None);
        }
        let (session_id, request_id) = match parse_cancel_request(invocation) {
            Ok(request) => request,
            Err(reply) => return reply,
        };
        let reply = match self.cancel_disposition(session_id, request_id) {
            Ok(disposition) => InvocationReply::success(json!({
                "request_id": request_id,
                "disposition": disposition,
            })),
            Err(reply) => reply,
        };
        enforce_deadline(invocation, started, reply)
    }

    fn cancel_disposition(
        &self,
        session_id: &str,
        request_id: &str,
    ) -> Result<&'static str, InvocationReply> {
        let state = self.state.lock().expect("runtime state");
        if !state.sessions.contains_key(session_id) {
            return Err(InvocationReply::error("session_not_found", None));
        }
        Ok(match state.cancellation_sessions.get(request_id) {
            Some(owner) if owner == session_id => {
                state
                    .cancellations
                    .get(request_id)
                    .expect("cancellation token")
                    .store(true, Ordering::SeqCst);
                "cancellation_requested"
            }
            Some(_) | None if state.terminal_requests.contains_key(request_id) => {
                "already_terminal"
            }
            _ => "unknown_request",
        })
    }

    pub(crate) fn export(&self, invocation: &Invocation, started: Instant) -> InvocationReply {
        if deadline_expired(invocation, started) {
            return InvocationReply::error("timed_out", None);
        }
        let request = match parse_export_request(invocation) {
            Ok(request) => request,
            Err(reply) => return reply,
        };
        enforce_deadline(invocation, started, self.export_request(&request))
    }

    fn export_request(&self, request: &ExportRequest) -> InvocationReply {
        let directory = match self.session_directory(&request.session_id) {
            Ok(directory) => directory,
            Err(reply) => return reply,
        };
        match self.artifacts.export(
            &directory,
            &request.destination,
            request.selected.as_deref(),
        ) {
            Ok(files) => export_success(request, files),
            Err(error) => InvocationReply::error("export_failed", Some(&error.to_string())),
        }
    }

    pub(crate) fn doctor(&self, invocation: &Invocation, started: Instant) -> InvocationReply {
        if deadline_expired(invocation, started) {
            return InvocationReply::error("timed_out", None);
        }
        if let Err(message) = Input::new(&invocation.input, &["session_id", "target_id"]) {
            return InvocationReply::error("invalid_request", Some(&message));
        }
        let adapter_diagnostics = self
            .adapters
            .iter()
            .map(|adapter| adapter.diagnostics())
            .collect::<Vec<_>>();
        let state = self.state.lock().expect("runtime state");
        enforce_deadline(
            invocation,
            started,
            InvocationReply::success(json!({
                "host": {"minimum_macos": "26.0", "supported": true},
                "daemon": {
                    "instance": self.daemon_instance,
                    "build_digest": manuvra_protocol::build_digest(),
                    "adapters": adapter_diagnostics,
                },
                "permissions": {"same_user_socket": true, "native_permissions": "not_required_for_chrome_cdp"},
                "sessions": state.sessions.values().map(|session| json!({
                    "session_id": session.id,
                    "target_id": session.target_id,
                    "role": session.role,
                    "mode": session.mode,
                })).collect::<Vec<_>>(),
                "warnings": doctor_warnings(
                    &self.startup_removed,
                    &self.startup_preserved,
                    &self.doctor_warnings,
                    &adapter_diagnostics,
                ),
            })),
        )
    }

    pub(crate) fn setup(&self, invocation: &Invocation, started: Instant) -> InvocationReply {
        if deadline_expired(invocation, started) {
            return InvocationReply::error("timed_out", None);
        }
        if let Err(message) = Input::new(&invocation.input, &[]) {
            return InvocationReply::error("invalid_request", Some(&message));
        }
        let deadline = started + Duration::from_millis(invocation.deadline_ms);
        let result = self
            .adapters
            .iter()
            .find_map(|adapter| adapter.setup_permissions(deadline));
        let reply = match result {
            Some(Ok(mut value)) => {
                value["installation"] = self.setup_installation.clone();
                InvocationReply::success(value)
            }
            Some(Err(error)) => InvocationReply::error(&error.code, error.message.as_deref()),
            None => InvocationReply::error(
                "command_unsupported",
                Some("no adapter owns macOS permission setup"),
            ),
        };
        enforce_deadline(invocation, started, reply)
    }

    fn session_directory(&self, session_id: &str) -> Result<std::path::PathBuf, InvocationReply> {
        self.state
            .lock()
            .expect("runtime state")
            .sessions
            .get(session_id)
            .filter(|session| session.state == SessionState::Active)
            .map(|session| session.directory.clone())
            .ok_or_else(|| InvocationReply::error("session_not_found", None))
    }
}

fn deadline_expired(invocation: &Invocation, started: Instant) -> bool {
    Instant::now() >= started + Duration::from_millis(invocation.deadline_ms)
}

fn enforce_deadline(
    invocation: &Invocation,
    started: Instant,
    reply: InvocationReply,
) -> InvocationReply {
    if deadline_expired(invocation, started) {
        InvocationReply::error("timed_out", None)
    } else {
        reply
    }
}

fn doctor_warnings(
    removed: &[PathBuf],
    preserved: &[PathBuf],
    injected: &[String],
    adapters: &[Value],
) -> Vec<String> {
    let mut warnings = Vec::new();
    if !removed.is_empty() {
        warnings.push("verified_orphans_removed".to_owned());
    }
    if !preserved.is_empty() {
        warnings.push("unverified_orphans_preserved".to_owned());
    }
    if chrome_endpoint_refused(adapters) {
        warnings.push("chrome_endpoint_refused".to_owned());
    }
    warnings.extend_from_slice(injected);
    warnings
}

fn chrome_endpoint_refused(adapters: &[Value]) -> bool {
    adapters.iter().any(|adapter| {
        adapter.get("kind").and_then(Value::as_str) == Some("chrome")
            && adapter
                .get("endpoints")
                .and_then(Value::as_object)
                .is_some_and(|endpoints| {
                    endpoints
                        .values()
                        .any(|status| status.as_str() == Some("refused"))
                })
    })
}

fn parse_usage_request(invocation: &Invocation) -> Result<(&str, Option<&Path>), InvocationReply> {
    let input = Input::new(&invocation.input, &["action", "destination"])
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    let action = input
        .string("action")
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    let destination = input
        .optional_string("destination")
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?
        .map(Path::new);
    Ok((action, destination))
}

fn parse_target_list_request(
    invocation: &Invocation,
) -> Result<TargetListFilter<'_>, InvocationReply> {
    let input = Input::new(&invocation.input, &["kind", "cursor", "limit"])
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    Ok(TargetListFilter {
        kind: parse_target_kind(&input)?,
        cursor: parse_cursor(&input)?,
        limit: parse_target_limit(&input)?,
    })
}

fn parse_target_kind<'a>(input: &Input<'a>) -> Result<Option<&'a str>, InvocationReply> {
    let kind = input
        .optional_string("kind")
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    if kind.is_some_and(|value| !matches!(value, "chrome" | "macos")) {
        Err(InvocationReply::error(
            "invalid_request",
            Some("kind must be chrome or macos"),
        ))
    } else {
        Ok(kind)
    }
}

fn parse_target_limit(input: &Input<'_>) -> Result<usize, InvocationReply> {
    match input.unsigned("limit", Some(10)) {
        Ok(limit @ 1..=10) => Ok(limit as usize),
        _ => Err(InvocationReply::error(
            "invalid_request",
            Some("limit must be between 1 and 10"),
        )),
    }
}

fn target_list_entry(state: &RuntimeState, target: &TargetDescriptor) -> Value {
    json!({
        "target_id": target.target_id,
        "generation": target.generation,
        "kind": target.kind,
        "owner": bounded_label(&target.owner),
        "title": target.title.as_deref().and_then(bounded_title),
        "capabilities": target.capabilities,
        "actor_lease": if actor_lease_held(state, target) { "held" } else { "available" },
    })
}

fn actor_lease_held(state: &RuntimeState, target: &TargetDescriptor) -> bool {
    state.leases.get(&target.target_id).is_some_and(|lease| {
        lease.target_generation == target.generation
            && (lease.expires_at > Instant::now() || lease.pinned > 0)
    })
}

fn new_open_session(
    session_id: &str,
    request: &OpenRequest,
    target: &TargetDescriptor,
    directory: PathBuf,
) -> Session {
    Session {
        id: session_id.to_owned(),
        target_id: request.target_id.clone(),
        target_generation: target.generation,
        role: request.role.clone(),
        mode: request.mode.clone(),
        directory,
        lease_ttl_ms: request.lease_ttl_ms,
        reference_namespace: opaque_id("n_"),
        reference_epoch: 0,
        frame_token: None,
        in_flight: 0,
        state: SessionState::Active,
    }
}

fn parse_close_request(invocation: &Invocation) -> Result<(&str, bool), InvocationReply> {
    let input = Input::new(&invocation.input, &["session_id", "cancel_running"])
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    let session_id = input
        .string("session_id")
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    let cancel_running = input
        .boolean("cancel_running", false)
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    Ok((session_id, cancel_running))
}

fn closing_in_flight(state: &RuntimeState, session_id: &str) -> Result<usize, InvocationReply> {
    match state.sessions.get(session_id) {
        Some(session) if session.state == SessionState::Active => Ok(session.in_flight),
        Some(_) => Err(InvocationReply::error("session_busy", None)),
        None => Err(InvocationReply::error("session_not_found", None)),
    }
}

fn cancel_session_requests(state: &RuntimeState, session_id: &str) {
    for (request_id, owner) in &state.cancellation_sessions {
        if owner == session_id
            && let Some(token) = state.cancellations.get(request_id)
        {
            token.store(true, Ordering::SeqCst);
        }
    }
}

fn idle_closing_session(
    state: &RuntimeState,
    session_id: &str,
) -> Result<Session, InvocationReply> {
    match state.sessions.get(session_id) {
        Some(session) if session.in_flight == 0 => Ok(session.clone()),
        Some(_) => Err(InvocationReply::error("session_busy", None)),
        None => Err(InvocationReply::error("session_not_found", None)),
    }
}

fn forget_closed_session(state: &mut RuntimeState, session_id: &str, target_id: &str) {
    state.sessions.remove(session_id);
    if state
        .leases
        .get(target_id)
        .is_some_and(|lease| lease.session_id == session_id)
    {
        state.leases.remove(target_id);
    }
}

fn parse_lease_request(invocation: &Invocation) -> Result<LeaseRequest<'_>, InvocationReply> {
    let input = Input::new(&invocation.input, &["session_id", "action", "ttl_ms"])
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    let session_id = input
        .string("session_id")
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    let action = match input.string("action") {
        Ok(value @ ("acquire" | "renew" | "release")) => value,
        _ => {
            return Err(InvocationReply::error(
                "invalid_request",
                Some("lease action must be acquire, renew, or release"),
            ));
        }
    };
    let requested_ttl = input
        .unsigned("ttl_ms", Some(0))
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    Ok(LeaseRequest {
        session_id,
        action,
        requested_ttl,
    })
}

fn parse_cancel_request(invocation: &Invocation) -> Result<(&str, &str), InvocationReply> {
    let input = Input::new(&invocation.input, &["session_id", "request_id"])
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    let session_id = input
        .string("session_id")
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    let request_id = input
        .string("request_id")
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    Ok((session_id, request_id))
}

fn export_success(
    request: &ExportRequest,
    files: Vec<crate::artifacts::PublishedArtifact>,
) -> InvocationReply {
    let public_files = if request.selected.is_none() {
        vec![json!({
            "kind": "manifest",
            "path": request.destination.join("manifest.json"),
            "artifact_count": files.len(),
        })]
    } else {
        files
            .iter()
            .map(|file| {
                json!({
                    "artifact_id": file.artifact_id,
                    "kind": file.kind,
                    "path": file.path,
                    "sha256": file.sha256,
                })
            })
            .collect::<Vec<_>>()
    };
    InvocationReply::success(json!({
        "session_id": request.session_id,
        "destination": request.destination,
        "files": public_files,
        "verified": true,
    }))
}

fn parse_open_request(invocation: &Invocation) -> Result<OpenRequest, InvocationReply> {
    let input = Input::new(
        &invocation.input,
        &["target_id", "role", "mode", "lease_ttl_ms"],
    )
    .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    let target_id = input
        .string("target_id")
        .map(str::to_owned)
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    let role = parse_role(&input)?;
    let mode = parse_mode(&input, "mode", ExecutionMode::Background)?;
    let lease_ttl_ms = input
        .unsigned("lease_ttl_ms", Some(120_000))
        .ok()
        .filter(|ttl| (10_000..=600_000).contains(ttl))
        .ok_or_else(|| {
            InvocationReply::error(
                "invalid_request",
                Some("lease_ttl_ms must be between 10000 and 600000"),
            )
        })?;
    Ok(OpenRequest {
        target_id,
        role,
        mode,
        lease_ttl_ms,
    })
}

fn parse_export_request(invocation: &Invocation) -> Result<ExportRequest, InvocationReply> {
    let input = Input::new(
        &invocation.input,
        &["session_id", "artifact_ids", "all", "destination"],
    )
    .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    let session_id = input
        .string("session_id")
        .map(str::to_owned)
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    let destination = absolute_export_destination(&input)?;
    let selected = exclusive_export_selection(&input)?;
    Ok(ExportRequest {
        session_id,
        destination,
        selected,
    })
}

fn absolute_export_destination(input: &Input<'_>) -> Result<PathBuf, InvocationReply> {
    let destination = input
        .string("destination")
        .map(PathBuf::from)
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    if destination.is_absolute() {
        Ok(destination)
    } else {
        Err(InvocationReply::error(
            "invalid_request",
            Some("destination must be absolute"),
        ))
    }
}

fn exclusive_export_selection(input: &Input<'_>) -> Result<Option<Vec<String>>, InvocationReply> {
    let all = input
        .boolean("all", false)
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    let selected = parse_artifact_ids(input.optional_value("artifact_ids"))
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    if all == selected.is_some() {
        Err(InvocationReply::error(
            "invalid_request",
            Some("choose exactly one of all or artifact_ids"),
        ))
    } else {
        Ok(selected)
    }
}

fn discovery_list(value: &Value) -> InvocationReply {
    match parse_registry_page(value) {
        Ok((cursor, limit)) => InvocationReply::success(registry_page(cursor, limit)),
        Err(reply) => reply,
    }
}

fn parse_registry_page(value: &Value) -> Result<(usize, usize), InvocationReply> {
    let input = Input::new(value, &["cursor", "limit"])
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    let cursor = parse_cursor(&input)?;
    let limit = match input.unsigned("limit", Some(10)) {
        Ok(limit @ 1..=10) => limit as usize,
        _ => {
            return Err(InvocationReply::error(
                "invalid_request",
                Some("invalid registry limit"),
            ));
        }
    };
    Ok((cursor, limit))
}

fn discovery_get(value: &Value) -> InvocationReply {
    let input = match Input::new(value, &["command"]) {
        Ok(input) => input,
        Err(message) => return InvocationReply::error("invalid_request", Some(&message)),
    };
    match input.string("command").ok().and_then(command_help) {
        Some(help) => InvocationReply::success(help),
        None => InvocationReply::error("unknown_command", None),
    }
}

fn discovery_schema(value: &Value) -> InvocationReply {
    let (command, key) = match parse_schema_request(value) {
        Ok(request) => request,
        Err(reply) => return reply,
    };
    match resolve_schema_pointer(command, key) {
        Some(pointer) => InvocationReply::success(pointer),
        None => InvocationReply::error(
            "internal_error",
            Some("installed schema pointer is invalid"),
        ),
    }
}

fn parse_schema_request(value: &Value) -> Result<(&Value, &'static str), InvocationReply> {
    let input = Input::new(value, &["command", "side"])
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    let command = input
        .string("command")
        .ok()
        .and_then(command_descriptor)
        .ok_or_else(|| InvocationReply::error("unknown_command", None))?;
    Ok((command, schema_side(&input)?))
}

fn schema_side(input: &Input<'_>) -> Result<&'static str, InvocationReply> {
    match input.string("side") {
        Ok("input") => Ok("input_schema"),
        Ok("result") => Ok("result_schema"),
        _ => Err(InvocationReply::error(
            "invalid_request",
            Some("side must be input or result"),
        )),
    }
}

fn resolve_schema_pointer(command: &Value, key: &str) -> Option<Value> {
    command[key]
        .as_str()
        .and_then(|reference| schema_pointer(reference).ok())
}

fn discovery_error(value: &Value) -> InvocationReply {
    let input = match Input::new(value, &["code"]) {
        Ok(input) => input,
        Err(message) => return InvocationReply::error("invalid_request", Some(&message)),
    };
    match input.string("code").ok().and_then(error_meta) {
        Some(meta) => InvocationReply::success(json!({
            "code": meta.code,
            "meaning": meta.meaning,
            "effects": meta.effects,
            "retry": meta.retry,
            "recovery": meta.recovery,
        })),
        None => InvocationReply::error("invalid_request", Some("unknown error code")),
    }
}

fn bounded_label(value: &str) -> String {
    value.chars().take(256).collect()
}

fn bounded_title(value: &str) -> Option<String> {
    let title = bounded_label(value);
    (!title.is_empty()).then_some(title)
}

fn parse_cursor(input: &Input<'_>) -> Result<usize, InvocationReply> {
    match input.optional_string("cursor") {
        Ok(None) => Ok(0),
        Ok(Some(cursor)) => cursor
            .parse::<usize>()
            .map_err(|_| InvocationReply::error("invalid_request", Some("cursor is invalid"))),
        Err(message) => Err(InvocationReply::error("invalid_request", Some(&message))),
    }
}

fn parse_role(input: &Input<'_>) -> Result<SessionRole, InvocationReply> {
    match input.optional_string("role") {
        Ok(None | Some("actor")) => Ok(SessionRole::Actor),
        Ok(Some("observer")) => Ok(SessionRole::Observer),
        Ok(Some(_)) => Err(InvocationReply::error(
            "invalid_request",
            Some("role must be actor or observer"),
        )),
        Err(message) => Err(InvocationReply::error("invalid_request", Some(&message))),
    }
}

pub(crate) fn parse_mode(
    input: &Input<'_>,
    key: &str,
    default: ExecutionMode,
) -> Result<ExecutionMode, InvocationReply> {
    match input.optional_string(key) {
        Ok(None) => Ok(default),
        Ok(Some("background")) => Ok(ExecutionMode::Background),
        Ok(Some("foreground")) => Ok(ExecutionMode::Foreground),
        Ok(Some(_)) => Err(InvocationReply::error(
            "invalid_request",
            Some("mode must be background or foreground"),
        )),
        Err(message) => Err(InvocationReply::error("invalid_request", Some(&message))),
    }
}

fn expire_target_lease(
    leases: &mut std::collections::HashMap<String, ActorLease>,
    target_id: &str,
    now: Instant,
) {
    if leases
        .get(target_id)
        .is_some_and(|lease| lease.expires_at <= now && lease.pinned == 0)
    {
        leases.remove(target_id);
    }
}

fn actor_session(state: &RuntimeState, session_id: &str) -> Result<Session, InvocationReply> {
    match state.sessions.get(session_id) {
        Some(session) if session.role == SessionRole::Actor => Ok(session.clone()),
        Some(_) => Err(InvocationReply::error("actor_lease_required", None)),
        None => Err(InvocationReply::error("session_not_found", None)),
    }
}

fn validated_lease_ttl(requested: u64, default: u64) -> Result<u64, InvocationReply> {
    let ttl = if requested == 0 { default } else { requested };
    if (10_000..=600_000).contains(&ttl) {
        Ok(ttl)
    } else {
        Err(InvocationReply::error(
            "invalid_request",
            Some("ttl_ms must be between 10000 and 600000"),
        ))
    }
}

fn apply_validated_lease_action(
    leases: &mut std::collections::HashMap<String, ActorLease>,
    session: &Session,
    action: &str,
    ttl: u64,
    now: Instant,
) -> InvocationReply {
    match action {
        "acquire" => acquire_lease(leases, session, ttl, now),
        "renew" => renew_lease(leases, session, ttl, now),
        "release" => release_lease(leases, session),
        _ => unreachable!(),
    }
}

fn acquire_lease(
    leases: &mut std::collections::HashMap<String, ActorLease>,
    session: &Session,
    ttl: u64,
    now: Instant,
) -> InvocationReply {
    if leases.contains_key(&session.target_id) {
        return InvocationReply::error("actor_lease_held", None);
    }
    leases.insert(
        session.target_id.clone(),
        ActorLease {
            session_id: session.id.clone(),
            target_generation: session.target_generation,
            ttl_ms: ttl,
            expires_at: now + Duration::from_millis(ttl),
            pinned: 0,
        },
    );
    InvocationReply::success(lease_json(&session.id, ttl, "held"))
}

fn renew_lease(
    leases: &mut std::collections::HashMap<String, ActorLease>,
    session: &Session,
    ttl: u64,
    now: Instant,
) -> InvocationReply {
    match leases.get_mut(&session.target_id) {
        Some(lease) if lease.session_id == session.id => {
            lease.ttl_ms = ttl;
            lease.expires_at = now + Duration::from_millis(ttl);
            InvocationReply::success(lease_json(&session.id, ttl, "held"))
        }
        _ => InvocationReply::error("actor_lease_expired", None),
    }
}

fn release_lease(
    leases: &mut std::collections::HashMap<String, ActorLease>,
    session: &Session,
) -> InvocationReply {
    match leases.get(&session.target_id) {
        Some(lease) if lease.session_id == session.id && lease.pinned > 0 => {
            InvocationReply::error("session_busy", None)
        }
        Some(lease) if lease.session_id == session.id => {
            leases.remove(&session.target_id);
            InvocationReply::success(json!({
                "session_id": session.id,
                "state": "released",
                "ttl_ms": null,
                "expires_at": null,
            }))
        }
        _ => InvocationReply::error("actor_lease_expired", None),
    }
}

fn lease_json(session_id: &str, ttl_ms: u64, state: &str) -> Value {
    json!({
        "session_id": session_id,
        "state": state,
        "ttl_ms": ttl_ms,
        "expires_at": rfc3339_after(Duration::from_millis(ttl_ms)),
    })
}

fn parse_artifact_ids(value: Option<&Value>) -> Result<Option<Vec<String>>, String> {
    let Some(value) = value else { return Ok(None) };
    let values = value
        .as_array()
        .ok_or_else(|| "artifact_ids must be an array".to_owned())?;
    if values.is_empty() {
        return Err("artifact_ids must not be empty".to_owned());
    }
    Ok(Some(string_artifact_ids(values)?))
}

fn string_artifact_ids(values: &[Value]) -> Result<Vec<String>, String> {
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| "artifact ID must be a string".to_owned())
        })
        .collect()
}

fn usage_error_reply(error: UsageError) -> InvocationReply {
    InvocationReply::error(error.code(), Some(&error.to_string()))
}

#[allow(dead_code)]
fn artifact_error_reply(error: ArtifactError) -> InvocationReply {
    InvocationReply::error("artifact_io_failed", Some(&error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{
        chrome_endpoint_refused, doctor_warnings, parse_artifact_ids, parse_export_request,
        parse_role, parse_schema_request,
    };
    use crate::validation::Input;
    use manuvra_protocol::Invocation;
    use serde_json::json;

    #[test]
    fn accepted_error_catalog_has_all_entries() {
        assert_eq!(manuvra_protocol::all_errors().len(), 46);
    }

    #[test]
    fn doctor_warns_only_when_a_chrome_endpoint_is_classified_refused() {
        let refused = [json!({
            "kind": "chrome",
            "endpoints": {"127.0.0.1:9222": "refused"}
        })];
        let reachable = [json!({
            "kind": "chrome",
            "endpoints": {"127.0.0.1:9222": "reachable"}
        })];
        let occupied = [json!({
            "kind": "chrome",
            "endpoints": {
                "127.0.0.1:9222": "listener answered but Chrome /json/list was not an array"
            }
        })];
        assert!(chrome_endpoint_refused(&refused));
        assert!(!chrome_endpoint_refused(&reachable));
        assert!(!chrome_endpoint_refused(&occupied));
        assert_eq!(
            doctor_warnings(&[], &[], &[], &refused),
            vec!["chrome_endpoint_refused".to_owned()]
        );
        assert!(doctor_warnings(&[], &[], &[], &occupied).is_empty());
        assert!(doctor_warnings(&[], &[], &[], &[]).is_empty());
    }

    #[test]
    fn parse_role_and_export_selection_keep_admission_results() {
        let actor_input = json!({});
        let actor = Input::new(&actor_input, &["role"]).unwrap();
        assert!(matches!(
            parse_role(&actor),
            Ok(crate::model::SessionRole::Actor)
        ));
        let observer_input = json!({"role": "observer"});
        let observer = Input::new(&observer_input, &["role"]).unwrap();
        assert!(matches!(
            parse_role(&observer),
            Ok(crate::model::SessionRole::Observer)
        ));
        let invalid_input = json!({"role": "admin"});
        let invalid = Input::new(&invalid_input, &["role"]).unwrap();
        assert_eq!(
            parse_role(&invalid).unwrap_err().value["error"]["code"],
            "invalid_request"
        );

        assert!(parse_artifact_ids(None).unwrap().is_none());
        assert_eq!(
            parse_artifact_ids(Some(&json!(["a_one"]))).unwrap(),
            Some(vec!["a_one".to_owned()])
        );
        assert!(parse_artifact_ids(Some(&json!([]))).is_err());
        assert!(parse_artifact_ids(Some(&json!("a_one"))).is_err());
        assert!(parse_artifact_ids(Some(&json!([1]))).is_err());

        let destination = std::env::temp_dir().join("manuvra-export-parse");
        let parsed = parse_export_request(&Invocation::new(
            "artifact.export",
            json!({
                "session_id": "s_test",
                "all": true,
                "destination": destination
            }),
            "export-parse".to_owned(),
            1_000,
        ))
        .unwrap();
        assert!(parsed.selected.is_none());
        assert!(
            parse_export_request(&Invocation::new(
                "artifact.export",
                json!({
                    "session_id": "s_test",
                    "destination": "relative"
                }),
                "export-relative".to_owned(),
                1_000,
            ))
            .is_err()
        );
    }

    #[test]
    fn discovery_schema_accepts_input_and_result_sides() {
        let input = json!({"command": "action.click", "side": "input"});
        let parsed = parse_schema_request(&input).unwrap();
        assert_eq!(parsed.1, "input_schema");
        let result = json!({"command": "action.click", "side": "result"});
        assert_eq!(parse_schema_request(&result).unwrap().1, "result_schema");
        assert_eq!(
            parse_schema_request(&json!({"command": "missing", "side": "input"}))
                .unwrap_err()
                .value["error"]["code"],
            "unknown_command"
        );
    }
}
