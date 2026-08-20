use crate::artifacts::ArtifactWrite;
use crate::model::{
    AdapterContext, AdapterDelivery, AdapterOperation, ExecutionMode, Session, SessionRole,
    SessionState, TargetDescriptor,
};
use crate::usage::UsageKey;
use crate::validation::Input;
use crate::{InvocationReply, Runtime, RuntimeState};
use manuvra_protocol::{
    Invocation, command_authority, command_capabilities, command_input_fields, command_modes,
    command_required_fields, operational_error,
};
use serde_json::{Value, json};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

struct CommandPlan {
    session_id: String,
    mode_override: Option<ExecutionMode>,
    raw: Option<RawUsage>,
    mutating: bool,
}

#[derive(Clone)]
struct RawUsage {
    backend: String,
    operation: String,
    intent: Option<String>,
}

struct Admission {
    context: AdapterContext,
    session_directory: PathBuf,
    request_id: String,
    cancellation: Arc<AtomicBool>,
    raw: Option<RawUsage>,
    lease_pinned: bool,
}

struct QueuedMutation {
    target_id: String,
    cancellation: Arc<AtomicBool>,
}

impl Runtime {
    pub(crate) fn execute_adapter_command(
        &self,
        invocation: &Invocation,
        started: Instant,
    ) -> InvocationReply {
        let plan = match plan_command(invocation) {
            Ok(plan) => plan,
            Err(reply) => return reply,
        };
        if !plan.mutating {
            return self.execute_raw_read(invocation, plan, started);
        }
        let queued = match self.queue_mutation(invocation, &plan, started) {
            Ok(queued) => queued,
            Err(reply) => return reply,
        };
        let target_lock = self.target_lock(&queued.target_id);
        let _target_guard = target_lock.blocking_lock();
        if queued.cancellation.load(Ordering::SeqCst) {
            self.finish_queued_mutation(invocation, &plan.session_id);
            return predispatch_action_error(invocation, &plan.session_id, "cancelled", started);
        }
        if request_deadline(started, invocation).is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.finish_queued_mutation(invocation, &plan.session_id);
            return predispatch_action_error(invocation, &plan.session_id, "timed_out", started);
        }
        let operation =
            match self.prepare_mutation(invocation, &plan, started, queued.cancellation.clone()) {
                Ok(operation) => operation,
                Err(reply) => {
                    self.finish_queued_mutation(invocation, &plan.session_id);
                    return reply;
                }
            };
        let admission =
            match self.admit_mutation(invocation, &plan, started, queued.cancellation.clone()) {
                Ok(admission) => admission,
                Err(reply) => {
                    self.finish_queued_mutation(invocation, &plan.session_id);
                    return reply;
                }
            };
        self.dispatch_admitted_mutation(invocation, started, admission, operation)
    }

    fn dispatch_admitted_mutation(
        &self,
        invocation: &Invocation,
        started: Instant,
        admission: Admission,
        operation: AdapterOperation,
    ) -> InvocationReply {
        let reply = match self
            .adapter_for_until(&admission.context.target_id, admission.context.deadline)
        {
            Ok(Some(adapter)) => match catch_unwind(AssertUnwindSafe(|| {
                adapter.invoke(
                    &admission.context,
                    &operation,
                    admission.cancellation.clone(),
                )
            })) {
                Ok(adapter_reply) => {
                    self.complete_mutation(invocation, started, &admission, adapter_reply)
                }
                Err(_) => self.finish_raw_usage(
                    &admission,
                    "uncertain",
                    admitted_action_error(invocation, &admission, "internal_error", started),
                ),
            },
            Ok(None) => admitted_action_error(invocation, &admission, "target_not_found", started),
            Err(error) => admitted_action_error(invocation, &admission, &error.code, started),
        };
        self.finish_admitted(&admission);
        reply
    }

    fn prepare_mutation(
        &self,
        invocation: &Invocation,
        plan: &CommandPlan,
        started: Instant,
        cancellation: Arc<AtomicBool>,
    ) -> Result<AdapterOperation, InvocationReply> {
        let (context, adapter) = {
            let state = self.state.lock().expect("runtime state");
            let session = state
                .sessions
                .get(&plan.session_id)
                .cloned()
                .ok_or_else(|| {
                    action_error(
                        invocation,
                        &plan.session_id,
                        "session_not_found",
                        None,
                        started,
                        None,
                    )
                })?;
            let mode = self.validate_mutation_context(invocation, plan, &session, started)?;
            let adapter = self
                .adapter_for_until(
                    &session.target_id,
                    started + Duration::from_millis(invocation.deadline_ms),
                )
                .map_err(|error| {
                    action_error(
                        invocation,
                        &session.id,
                        &error.code,
                        Some(&mode),
                        started,
                        None,
                    )
                })?
                .ok_or_else(|| {
                    action_error(
                        invocation,
                        &session.id,
                        "target_not_found",
                        Some(&mode),
                        started,
                        None,
                    )
                })?;
            let action_sequence = state
                .action_sequences
                .get(&session.target_id)
                .copied()
                .unwrap_or(0)
                .saturating_add(1);
            (
                AdapterContext {
                    session_id: session.id,
                    target_id: session.target_id,
                    target_generation: session.target_generation,
                    action_sequence,
                    reference_namespace: session.reference_namespace,
                    reference_epoch: session.reference_epoch,
                    frame_token: session.frame_token,
                    mode,
                    deadline: started + Duration::from_millis(invocation.deadline_ms),
                },
                adapter,
            )
        };
        let operation = AdapterOperation::new(invocation.command.clone(), invocation.input.clone());
        let mode = context.mode.clone();
        match catch_unwind(AssertUnwindSafe(|| {
            adapter.prepare(&context, &operation, cancellation)
        })) {
            Ok(Ok(prepared)) => Ok(prepared),
            Ok(Err(error)) => Err(predispatch_adapter_error(
                invocation,
                &plan.session_id,
                &error.code,
                error.message.as_deref(),
                &mode,
                started,
            )),
            Err(_) => Err(predispatch_action_error(
                invocation,
                &plan.session_id,
                "internal_error",
                started,
            )),
        }
    }

    fn execute_raw_read(
        &self,
        invocation: &Invocation,
        plan: CommandPlan,
        started: Instant,
    ) -> InvocationReply {
        let admission = match self.admit_read(invocation, &plan, started) {
            Ok(admission) => admission,
            Err(reply) => return reply,
        };
        let adapter = match self
            .adapter_for_until(&admission.context.target_id, admission.context.deadline)
        {
            Ok(Some(adapter)) => adapter,
            Ok(None) => {
                self.finish_admitted(&admission);
                return InvocationReply::error("target_not_found", None);
            }
            Err(error) => {
                self.finish_admitted(&admission);
                return InvocationReply::error(&error.code, error.message.as_deref());
            }
        };
        let operation = AdapterOperation::new(invocation.command.clone(), invocation.input.clone());
        let adapter_reply = match catch_unwind(AssertUnwindSafe(|| {
            adapter.invoke(
                &admission.context,
                &operation,
                admission.cancellation.clone(),
            )
        })) {
            Ok(reply) => reply,
            Err(_) => {
                let reply = self.finish_raw_usage(
                    &admission,
                    "uncertain",
                    InvocationReply::error("internal_error", None),
                );
                self.finish_admitted(&admission);
                return reply;
            }
        };
        let reply = self.complete_raw_read(invocation, started, &admission, adapter_reply);
        self.finish_admitted(&admission);
        reply
    }

    fn admit_mutation(
        &self,
        invocation: &Invocation,
        plan: &CommandPlan,
        started: Instant,
        cancellation: Arc<AtomicBool>,
    ) -> Result<Admission, InvocationReply> {
        let now = Instant::now();
        let mut state = self.state.lock().expect("runtime state");
        let session = state
            .sessions
            .get(&plan.session_id)
            .cloned()
            .ok_or_else(|| {
                action_error(
                    invocation,
                    &plan.session_id,
                    "session_not_found",
                    None,
                    started,
                    None,
                )
            })?;
        let mode = self.validate_mutation_context(invocation, plan, &session, started)?;
        let action_sequence =
            pin_actor_lease(&mut state, invocation, &session, &mode, started, now)?;
        let active = state.sessions.get_mut(&session.id).expect("session exists");
        active.reference_epoch += 1;
        active.frame_token = None;
        Ok(Admission {
            context: AdapterContext {
                session_id: session.id,
                target_id: session.target_id,
                target_generation: session.target_generation,
                action_sequence,
                reference_namespace: session.reference_namespace.clone(),
                reference_epoch: session.reference_epoch,
                frame_token: session.frame_token.clone(),
                mode,
                deadline: started + Duration::from_millis(invocation.deadline_ms),
            },
            session_directory: session.directory,
            request_id: invocation.request_id.clone(),
            cancellation,
            raw: plan.raw.clone(),
            lease_pinned: true,
        })
    }

    fn queue_mutation(
        &self,
        invocation: &Invocation,
        plan: &CommandPlan,
        started: Instant,
    ) -> Result<QueuedMutation, InvocationReply> {
        let mut state = self.state.lock().expect("runtime state");
        let session = state
            .sessions
            .get(&plan.session_id)
            .cloned()
            .ok_or_else(|| {
                action_error(
                    invocation,
                    &plan.session_id,
                    "session_not_found",
                    None,
                    started,
                    None,
                )
            })?;
        if session.state == SessionState::Closing {
            return Err(action_error(
                invocation,
                &plan.session_id,
                "session_busy",
                None,
                started,
                None,
            ));
        }
        let cancellation = Arc::new(AtomicBool::new(false));
        state
            .sessions
            .get_mut(&session.id)
            .expect("queued session")
            .in_flight += 1;
        state
            .cancellations
            .insert(invocation.request_id.clone(), cancellation.clone());
        state
            .cancellation_sessions
            .insert(invocation.request_id.clone(), session.id);
        Ok(QueuedMutation {
            target_id: session.target_id,
            cancellation,
        })
    }

    fn finish_queued_mutation(&self, invocation: &Invocation, session_id: &str) {
        let mut state = self.state.lock().expect("runtime state");
        state.cancellations.remove(&invocation.request_id);
        state.cancellation_sessions.remove(&invocation.request_id);
        if let Some(session) = state.sessions.get_mut(session_id) {
            session.in_flight = session.in_flight.saturating_sub(1);
        }
    }

    fn validate_mutation_context(
        &self,
        invocation: &Invocation,
        plan: &CommandPlan,
        session: &Session,
        started: Instant,
    ) -> Result<ExecutionMode, InvocationReply> {
        validate_session_ready(invocation, session, started)?;
        let target = self
            .target_until(
                &session.target_id,
                started + Duration::from_millis(invocation.deadline_ms),
            )
            .map_err(|error| {
                action_error(
                    invocation,
                    &plan.session_id,
                    &error.code,
                    None,
                    started,
                    None,
                )
            })?
            .ok_or_else(|| {
                action_error(
                    invocation,
                    &plan.session_id,
                    "target_not_found",
                    None,
                    started,
                    None,
                )
            })?;
        validate_target(invocation, session, &target, started)?;
        let mode = plan
            .mode_override
            .clone()
            .unwrap_or_else(|| session.mode.clone());
        validate_background(invocation, &target, &mode).map_err(|code| {
            action_error(
                invocation,
                &plan.session_id,
                code,
                Some(&mode),
                started,
                None,
            )
        })?;
        validate_locator(invocation, session).map_err(|code| {
            action_error(
                invocation,
                &plan.session_id,
                code,
                Some(&mode),
                started,
                None,
            )
        })?;
        Ok(mode)
    }

    fn admit_read(
        &self,
        invocation: &Invocation,
        plan: &CommandPlan,
        started: Instant,
    ) -> Result<Admission, InvocationReply> {
        let mut state = self.state.lock().expect("runtime state");
        let session = state
            .sessions
            .get(&plan.session_id)
            .cloned()
            .ok_or_else(|| InvocationReply::error("session_not_found", None))?;
        if session.state == SessionState::Closing {
            return Err(InvocationReply::error("session_busy", None));
        }
        let target = self
            .target_until(
                &session.target_id,
                started + Duration::from_millis(invocation.deadline_ms),
            )
            .map_err(|error| InvocationReply::error(&error.code, error.message.as_deref()))?
            .ok_or_else(|| InvocationReply::error("target_not_found", None))?;
        if target.generation != session.target_generation {
            return Err(InvocationReply::error("target_stale", None));
        }
        validate_capability(invocation, &target)
            .map_err(|code| InvocationReply::error(code, None))?;
        validate_read_authority(invocation, &session, &state)?;
        validate_locator(invocation, &session)
            .map_err(|code| InvocationReply::error(code, None))?;
        let mode = plan
            .mode_override
            .clone()
            .unwrap_or_else(|| session.mode.clone());
        let lease_pinned = invocation.command == "raw.cdp";
        if lease_pinned {
            let lease = state
                .leases
                .get_mut(&session.target_id)
                .expect("validated actor lease");
            lease.expires_at = Instant::now() + Duration::from_millis(lease.ttl_ms);
            lease.pinned += 1;
        }
        state
            .sessions
            .get_mut(&session.id)
            .expect("session exists")
            .in_flight += 1;
        let cancellation = Arc::new(AtomicBool::new(false));
        state
            .cancellations
            .insert(invocation.request_id.clone(), cancellation.clone());
        state
            .cancellation_sessions
            .insert(invocation.request_id.clone(), session.id.clone());
        Ok(Admission {
            context: AdapterContext {
                session_id: session.id,
                target_id: session.target_id,
                target_generation: session.target_generation,
                action_sequence: *state.action_sequences.get(&target.target_id).unwrap_or(&0),
                reference_namespace: session.reference_namespace,
                reference_epoch: session.reference_epoch,
                frame_token: session.frame_token,
                mode,
                deadline: started + Duration::from_millis(invocation.deadline_ms),
            },
            session_directory: session.directory,
            request_id: invocation.request_id.clone(),
            cancellation,
            raw: plan.raw.clone(),
            lease_pinned,
        })
    }

    fn complete_mutation(
        &self,
        invocation: &Invocation,
        started: Instant,
        admission: &Admission,
        adapter_reply: crate::model::AdapterReply,
    ) -> InvocationReply {
        if adapter_reply.delivery == AdapterDelivery::Rejected {
            return self.complete_rejected_mutation(invocation, started, admission, &adapter_reply);
        }
        if adapter_reply.delivery == AdapterDelivery::Unknown {
            return self.complete_unknown_mutation(invocation, started, admission, &adapter_reply);
        }
        if let Some(raw) = &admission.raw {
            if self
                .publish_raw_response(admission, &adapter_reply.response)
                .is_err()
            {
                return self.finish_raw_usage(
                    admission,
                    "uncertain",
                    admitted_action_error(invocation, admission, "artifact_io_failed", started),
                );
            }
            debug_assert!(!raw.operation.is_empty());
        }
        if adapter_reply.interrupted {
            let code = adapter_reply
                .error
                .as_ref()
                .map(|error| error.code.as_str())
                .unwrap_or("interrupted");
            return self.finish_raw_usage(
                admission,
                "uncertain",
                admitted_action_error(invocation, admission, code, started),
            );
        }
        if !adapter_reply.already_settled
            && let Err(code) = settle(admission, &adapter_reply)
        {
            return self.finish_raw_usage(
                admission,
                "uncertain",
                admitted_action_error(invocation, admission, code, started),
            );
        }
        let Some(screenshot) = adapter_reply.screenshot.as_deref() else {
            return self.finish_raw_usage(
                admission,
                "uncertain",
                admitted_action_error(invocation, admission, "capture_failed", started),
            );
        };
        let artifact_started = Instant::now();
        let published = match self.artifacts.publish(
            &admission.session_directory,
            ArtifactWrite {
                kind: "screenshot",
                extension: "png",
                media_type: "image/png",
                bytes: screenshot,
                request_id: &admission.request_id,
                action_sequence: admission.context.action_sequence,
            },
        ) {
            Ok(published) => published,
            Err(error) => {
                return self.finish_raw_usage(
                    admission,
                    "uncertain",
                    admitted_action_error_with_message(
                        invocation,
                        admission,
                        "artifact_io_failed",
                        &error.to_string(),
                        started,
                    ),
                );
            }
        };
        let artifact_ms = millis(artifact_started.elapsed());
        let elapsed = millis(started.elapsed());
        let signature = adapter_reply
            .frame_signature
            .as_deref()
            .unwrap_or("unbound");
        let frame_token = format!(
            "f_{}_{}_{}_{}",
            admission.context.session_id,
            admission.context.target_generation,
            admission.context.action_sequence,
            signature
        );
        self.set_action_frame_token(&admission.context.session_id, frame_token.clone());
        let result = InvocationReply::success(json!({
            "schema": "manuvra/action-result@1",
            "protocol_version": "1.0",
            "registry_version": "1.0.0",
            "request_id": invocation.request_id,
            "command": invocation.command,
            "session_id": admission.context.session_id,
            "target_id": admission.context.target_id,
            "action_sequence": admission.context.action_sequence,
            "outcome": "observed",
            "delivery": "backend_confirmed",
            "requested_mode": admission.context.mode.as_str(),
            "effective_mode": admission.context.mode.as_str(),
            "effect_verification": "not_asserted",
            "observation": {
                "status": "captured",
                "screenshot_path": published.path,
                "frame_token": frame_token,
                "action_sequence_before": admission.context.action_sequence,
                "action_sequence_after": admission.context.action_sequence,
            },
            "timing_ms": adapter_timing(elapsed, artifact_ms, &adapter_reply.timing),
            "warnings": [],
            "error": null,
            "manifest_path": published.manifest_path,
        }));
        self.finish_raw_usage(admission, "completed", result)
    }

    fn complete_rejected_mutation(
        &self,
        invocation: &Invocation,
        started: Instant,
        admission: &Admission,
        adapter_reply: &crate::model::AdapterReply,
    ) -> InvocationReply {
        let code = adapter_reply
            .error
            .as_ref()
            .map(|error| error.code.as_str())
            .unwrap_or("backend_rejected");
        let mut result = admitted_action_error_with_optional_message(
            invocation,
            admission,
            code,
            adapter_reply
                .error
                .as_ref()
                .and_then(|error| error.message.as_deref()),
            started,
        );
        result.value["outcome"] = Value::String("not_performed".to_owned());
        result.value["observation"]["status"] = Value::String("not_attempted".to_owned());
        result.value["error"]["effects"] = Value::String("none".to_owned());
        if code != "dispatch_failed" {
            result.value["delivery"] = Value::String("backend_rejected".to_owned());
        }
        if admission.raw.is_some() {
            match self.publish_raw_response(admission, &adapter_reply.response) {
                Ok(manifest_path) => result.value["manifest_path"] = json!(manifest_path),
                Err(message) => {
                    result = admitted_action_error_with_message(
                        invocation,
                        admission,
                        "artifact_io_failed",
                        &message,
                        started,
                    );
                }
            }
        }
        self.finish_raw_usage(admission, "not_performed", result)
    }

    fn complete_unknown_mutation(
        &self,
        invocation: &Invocation,
        started: Instant,
        admission: &Admission,
        adapter_reply: &crate::model::AdapterReply,
    ) -> InvocationReply {
        let code = if let Some(error) = &adapter_reply.error {
            error.code.as_str()
        } else if admission.cancellation.load(Ordering::SeqCst) {
            "cancelled"
        } else if Instant::now() >= admission.context.deadline {
            "timed_out"
        } else {
            "transport_ambiguous"
        };
        self.finish_raw_usage(
            admission,
            "uncertain",
            admitted_action_error_with_optional_message(
                invocation,
                admission,
                code,
                adapter_reply
                    .error
                    .as_ref()
                    .and_then(|error| error.message.as_deref()),
                started,
            ),
        )
    }

    fn publish_raw_response(
        &self,
        admission: &Admission,
        response: &Value,
    ) -> Result<PathBuf, String> {
        let bytes = serde_json::to_vec(response).expect("adapter response");
        self.artifacts
            .publish(
                &admission.session_directory,
                ArtifactWrite {
                    kind: "raw_response",
                    extension: "json",
                    media_type: "application/json",
                    bytes: &bytes,
                    request_id: &admission.request_id,
                    action_sequence: admission.context.action_sequence,
                },
            )
            .map(|published| published.manifest_path)
            .map_err(|error| error.to_string())
    }

    fn set_action_frame_token(&self, session_id: &str, frame_token: String) {
        if let Some(session) = self
            .state
            .lock()
            .expect("runtime state")
            .sessions
            .get_mut(session_id)
        {
            session.frame_token = Some(frame_token);
        }
    }

    fn complete_raw_read(
        &self,
        invocation: &Invocation,
        started: Instant,
        admission: &Admission,
        adapter_reply: crate::model::AdapterReply,
    ) -> InvocationReply {
        let outcome = match adapter_reply.delivery {
            AdapterDelivery::Confirmed => "completed",
            AdapterDelivery::Rejected => "not_performed",
            AdapterDelivery::Unknown => "uncertain",
        };
        let response_value = adapter_reply.error.as_ref().map_or_else(
            || adapter_reply.response.clone(),
            |error| {
                json!({
                    "error": {
                        "code": error.code,
                        "message": error.message,
                        "details": error.details,
                    },
                    "value": adapter_reply.response,
                })
            },
        );
        let response = serde_json::to_vec(&response_value).expect("adapter response");
        let published = match self.artifacts.publish(
            &admission.session_directory,
            ArtifactWrite {
                kind: "raw_response",
                extension: "json",
                media_type: "application/json",
                bytes: &response,
                request_id: &admission.request_id,
                action_sequence: admission.context.action_sequence,
            },
        ) {
            Ok(published) => published,
            Err(error) => {
                return InvocationReply::error("artifact_io_failed", Some(&error.to_string()));
            }
        };
        let delivery = match adapter_reply.delivery {
            AdapterDelivery::Confirmed => "backend_confirmed",
            AdapterDelivery::Rejected => "backend_rejected",
            AdapterDelivery::Unknown => "unknown",
        };
        let result = InvocationReply::success(json!({
            "session_id": admission.context.session_id,
            "command": invocation.command,
            "intent": admission.raw.as_ref().and_then(|raw| raw.intent.clone()),
            "delivery": delivery,
            "response_path": published.path,
            "timing_ms": {"total": millis(started.elapsed())},
            "warning": null,
        }));
        self.finish_raw_usage(admission, outcome, result)
    }

    fn finish_raw_usage(
        &self,
        admission: &Admission,
        outcome: &str,
        mut result: InvocationReply,
    ) -> InvocationReply {
        let Some(raw) = &admission.raw else {
            return result;
        };
        let key = UsageKey {
            backend: &raw.backend,
            operation: &raw.operation,
            intent: raw.intent.as_deref(),
            outcome,
        };
        if self.usage.record(key).is_err() {
            add_usage_warning(&mut result.value);
        }
        result
    }

    fn finish_admitted(&self, admission: &Admission) {
        let mut state = self.state.lock().expect("runtime state");
        state.cancellations.remove(&admission.request_id);
        state.cancellation_sessions.remove(&admission.request_id);
        if let Some(session) = state.sessions.get_mut(&admission.context.session_id) {
            session.in_flight = session.in_flight.saturating_sub(1);
        }
        if admission.lease_pinned
            && let Some(lease) = state.leases.get_mut(&admission.context.target_id)
            && lease.session_id == admission.context.session_id
        {
            lease.pinned = lease.pinned.saturating_sub(1);
        }
    }
}

fn validate_session_ready(
    invocation: &Invocation,
    session: &Session,
    started: Instant,
) -> Result<(), InvocationReply> {
    if session.state == SessionState::Closing {
        return Err(action_error(
            invocation,
            &session.id,
            "session_busy",
            None,
            started,
            None,
        ));
    }
    if command_authority(&invocation.command) == Some("actor") && session.role != SessionRole::Actor
    {
        return Err(action_error(
            invocation,
            &session.id,
            "actor_lease_required",
            None,
            started,
            None,
        ));
    }
    Ok(())
}

fn request_deadline(started: Instant, invocation: &Invocation) -> Option<Instant> {
    started.checked_add(Duration::from_millis(invocation.deadline_ms))
}

fn predispatch_action_error(
    invocation: &Invocation,
    session_id: &str,
    code: &str,
    started: Instant,
) -> InvocationReply {
    let mut reply = action_error(invocation, session_id, code, None, started, None);
    reply.value["outcome"] = Value::String("not_performed".to_owned());
    reply.value["delivery"] = Value::String("not_dispatched".to_owned());
    reply.value["error"]["effects"] = Value::String("none".to_owned());
    reply.value["error"]["phase"] = Value::String("preflight".to_owned());
    reply.value["error"]["retry"] = Value::String("immediate".to_owned());
    reply
}

fn predispatch_adapter_error(
    invocation: &Invocation,
    session_id: &str,
    code: &str,
    message: Option<&str>,
    mode: &ExecutionMode,
    started: Instant,
) -> InvocationReply {
    let mut reply = action_error_with_optional_message(
        invocation,
        session_id,
        code,
        message,
        Some(mode),
        started,
        None,
    );
    reply.value["outcome"] = Value::String("not_performed".to_owned());
    reply.value["delivery"] = Value::String("not_dispatched".to_owned());
    reply.value["error"]["effects"] = Value::String("none".to_owned());
    reply.value["error"]["phase"] = Value::String("preflight".to_owned());
    reply
}

fn validate_target(
    invocation: &Invocation,
    session: &Session,
    target: &TargetDescriptor,
    started: Instant,
) -> Result<(), InvocationReply> {
    if target.generation != session.target_generation {
        return Err(action_error(
            invocation,
            &session.id,
            "target_stale",
            None,
            started,
            None,
        ));
    }
    validate_capability(invocation, target)
        .map_err(|code| action_error(invocation, &session.id, code, None, started, None))
}

fn pin_actor_lease(
    state: &mut RuntimeState,
    invocation: &Invocation,
    session: &Session,
    mode: &ExecutionMode,
    started: Instant,
    now: Instant,
) -> Result<u64, InvocationReply> {
    let lease = state.leases.get(&session.target_id).ok_or_else(|| {
        action_error(
            invocation,
            &session.id,
            "actor_lease_expired",
            Some(mode),
            started,
            None,
        )
    })?;
    if lease.session_id != session.id {
        return Err(action_error(
            invocation,
            &session.id,
            "actor_lease_required",
            Some(mode),
            started,
            None,
        ));
    }
    if lease.target_generation != session.target_generation {
        return Err(action_error(
            invocation,
            &session.id,
            "target_stale",
            Some(mode),
            started,
            None,
        ));
    }
    if lease.expires_at <= now && lease.pinned == 0 {
        state.leases.remove(&session.target_id);
        return Err(action_error(
            invocation,
            &session.id,
            "actor_lease_expired",
            Some(mode),
            started,
            None,
        ));
    }
    let lease = state
        .leases
        .get_mut(&session.target_id)
        .expect("validated lease");
    lease.expires_at = now + Duration::from_millis(lease.ttl_ms);
    lease.pinned += 1;
    let sequence = state
        .action_sequences
        .entry(session.target_id.clone())
        .or_insert(0);
    *sequence += 1;
    Ok(*sequence)
}

fn plan_command(invocation: &Invocation) -> Result<CommandPlan, InvocationReply> {
    let allowed = command_input_fields(&invocation.command)
        .ok_or_else(|| InvocationReply::error("unknown_command", None))?;
    let input = Input::new(&invocation.input, &allowed)
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    let session_id = input
        .string("session_id")
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?
        .to_owned();
    validate_command_specific(invocation, &input)?;
    let mode_override = if allowed.contains(&"mode") {
        match input.optional_string("mode") {
            Ok(None) => None,
            Ok(Some(mode @ ("background" | "foreground")))
                if command_modes(&invocation.command)
                    .is_some_and(|modes| modes.contains(&mode)) =>
            {
                Some(if mode == "background" {
                    ExecutionMode::Background
                } else {
                    ExecutionMode::Foreground
                })
            }
            Ok(Some(_)) => {
                return Err(InvocationReply::error(
                    "invalid_request",
                    Some("invalid mode"),
                ));
            }
            Err(message) => return Err(InvocationReply::error("invalid_request", Some(&message))),
        }
    } else {
        None
    };
    let (raw, mutating) = raw_and_effect(invocation, &input)?;
    Ok(CommandPlan {
        session_id,
        mode_override,
        raw,
        mutating,
    })
}

fn validate_command_specific(
    invocation: &Invocation,
    input: &Input<'_>,
) -> Result<(), InvocationReply> {
    validate_required_fields(invocation, input)?;
    if invocation.command == "raw.cdp" {
        validate_cdp(input)?;
    }
    Ok(())
}

fn validate_required_fields(
    invocation: &Invocation,
    input: &Input<'_>,
) -> Result<(), InvocationReply> {
    for key in command_required_fields(&invocation.command)
        .ok_or_else(|| InvocationReply::error("unknown_command", None))?
    {
        input
            .value(key)
            .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    }
    Ok(())
}

fn validate_cdp(input: &Input<'_>) -> Result<(), InvocationReply> {
    if !matches!(input.string("intent"), Ok("query" | "action")) {
        return Err(InvocationReply::error(
            "invalid_request",
            Some("raw CDP intent must be query or action"),
        ));
    }
    if !input.value("params").is_ok_and(|value| value.is_object()) {
        return Err(InvocationReply::error(
            "invalid_request",
            Some("raw CDP params must be an object"),
        ));
    }
    Ok(())
}

fn raw_and_effect(
    invocation: &Invocation,
    input: &Input<'_>,
) -> Result<(Option<RawUsage>, bool), InvocationReply> {
    match invocation.command.as_str() {
        "raw.cdp" => {
            let intent = input.string("intent").expect("validated intent");
            let method = input
                .string("method")
                .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
            Ok((
                Some(RawUsage {
                    backend: "cdp".to_owned(),
                    operation: method.to_owned(),
                    intent: Some(intent.to_owned()),
                }),
                intent == "action",
            ))
        }
        "raw.ax.get" => Ok((Some(ax_usage(input, "attribute")?), false)),
        "raw.ax.set" => Ok((Some(ax_usage(input, "attribute")?), true)),
        "raw.ax.perform" => Ok((Some(ax_usage(input, "action")?), true)),
        _ => Ok((None, true)),
    }
}

fn ax_usage(input: &Input<'_>, key: &str) -> Result<RawUsage, InvocationReply> {
    let operation = input
        .string(key)
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))?;
    Ok(RawUsage {
        backend: "ax".to_owned(),
        operation: operation.to_owned(),
        intent: None,
    })
}

fn validate_capability(
    invocation: &Invocation,
    target: &TargetDescriptor,
) -> Result<(), &'static str> {
    let required = command_capabilities(&invocation.command).ok_or("command_unsupported")?;
    required
        .iter()
        .all(|capability| {
            target
                .capabilities
                .iter()
                .any(|candidate| candidate == capability)
        })
        .then_some(())
        .ok_or("capability_unavailable")
}

fn validate_background(
    invocation: &Invocation,
    target: &TargetDescriptor,
    mode: &ExecutionMode,
) -> Result<(), &'static str> {
    let requires_foreground = target.kind == "macos"
        && matches!(
            invocation.command.as_str(),
            "action.press" | "action.scroll"
        );
    if requires_foreground && mode == &ExecutionMode::Background {
        return Err("foreground_required");
    }
    Ok(())
}

fn validate_locator(
    invocation: &Invocation,
    session: &crate::model::Session,
) -> Result<(), &'static str> {
    let Some(locator) = invocation.input.get("locator") else {
        return validate_raw_ref(invocation, session);
    };
    let kind = locator
        .get("kind")
        .and_then(Value::as_str)
        .ok_or("invalid_request")?;
    match kind {
        "semantic" => validate_semantic(locator),
        "ref" => validate_reference(locator.get("ref").and_then(Value::as_str), session),
        "point" => validate_frame(locator.get("frame_token").and_then(Value::as_str), session),
        _ => Err("invalid_request"),
    }
}

fn validate_raw_ref(
    invocation: &Invocation,
    session: &crate::model::Session,
) -> Result<(), &'static str> {
    if !invocation.command.starts_with("raw.ax") {
        return Ok(());
    }
    validate_reference(invocation.input.get("ref").and_then(Value::as_str), session)
}

fn validate_semantic(locator: &Value) -> Result<(), &'static str> {
    let name = locator
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if name == "Missing" {
        return Err("element_not_found");
    }
    if name == "Ambiguous" {
        return Err("ambiguous_target");
    }
    Ok(())
}

fn validate_reference(
    reference: Option<&str>,
    session: &crate::model::Session,
) -> Result<(), &'static str> {
    let expected = format!(
        "e_{}_{}_",
        session.reference_namespace, session.reference_epoch
    );
    reference
        .filter(|value| value.starts_with(&expected))
        .map(|_| ())
        .ok_or("element_stale")
}

fn validate_frame(
    frame: Option<&str>,
    session: &crate::model::Session,
) -> Result<(), &'static str> {
    match (&session.frame_token, frame) {
        (Some(expected), Some(actual)) if expected == actual => Ok(()),
        _ => Err("frame_stale"),
    }
}

fn validate_read_authority(
    invocation: &Invocation,
    session: &crate::model::Session,
    state: &crate::RuntimeState,
) -> Result<(), InvocationReply> {
    if command_authority(&invocation.command) != Some("actor") {
        return Ok(());
    }
    if session.role != SessionRole::Actor {
        return Err(InvocationReply::error("actor_lease_required", None));
    }
    match state.leases.get(&session.target_id) {
        Some(lease)
            if lease.session_id == session.id
                && (lease.expires_at > Instant::now() || lease.pinned > 0) =>
        {
            Ok(())
        }
        _ => Err(InvocationReply::error("actor_lease_expired", None)),
    }
}

fn settle(admission: &Admission, reply: &crate::model::AdapterReply) -> Result<(), &'static str> {
    if reply.continuous_events {
        while Instant::now() < admission.context.deadline {
            if admission.cancellation.load(Ordering::SeqCst) {
                return Err("cancelled");
            }
            thread::sleep(Duration::from_millis(2));
        }
        return Err("stabilization_timeout");
    }
    if let Some(delay) = reply.relevant_event_after_ms {
        wait_cancellable(admission, Duration::from_millis(delay))?;
    }
    wait_cancellable(admission, Duration::from_millis(50))?;
    if reply.capture_race_once {
        wait_cancellable(admission, Duration::from_millis(50))?;
    }
    if Instant::now() > admission.context.deadline {
        return Err("stabilization_timeout");
    }
    Ok(())
}

fn wait_cancellable(admission: &Admission, duration: Duration) -> Result<(), &'static str> {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        if admission.cancellation.load(Ordering::SeqCst) {
            return Err("cancelled");
        }
        if Instant::now() >= admission.context.deadline {
            return Err("stabilization_timeout");
        }
        let remaining = until.saturating_duration_since(Instant::now());
        thread::sleep(remaining.min(Duration::from_millis(2)));
    }
    Ok(())
}

fn action_error(
    invocation: &Invocation,
    session_id: &str,
    code: &str,
    mode: Option<&ExecutionMode>,
    started: Instant,
    sequence: Option<u64>,
) -> InvocationReply {
    action_error_with_optional_message(invocation, session_id, code, None, mode, started, sequence)
}

fn admitted_action_error(
    invocation: &Invocation,
    admission: &Admission,
    code: &str,
    started: Instant,
) -> InvocationReply {
    let mut reply = action_error(
        invocation,
        &admission.context.session_id,
        code,
        Some(&admission.context.mode),
        started,
        Some(admission.context.action_sequence),
    );
    reply.value["target_id"] = Value::String(admission.context.target_id.clone());
    reply
}

fn admitted_action_error_with_message(
    invocation: &Invocation,
    admission: &Admission,
    code: &str,
    message: &str,
    started: Instant,
) -> InvocationReply {
    let mut reply = action_error_with_message(
        invocation,
        &admission.context.session_id,
        code,
        message,
        &admission.context.mode,
        started,
        admission.context.action_sequence,
    );
    reply.value["target_id"] = Value::String(admission.context.target_id.clone());
    reply
}

fn admitted_action_error_with_optional_message(
    invocation: &Invocation,
    admission: &Admission,
    code: &str,
    message: Option<&str>,
    started: Instant,
) -> InvocationReply {
    let mut reply = action_error_with_optional_message(
        invocation,
        &admission.context.session_id,
        code,
        message,
        Some(&admission.context.mode),
        started,
        Some(admission.context.action_sequence),
    );
    reply.value["target_id"] = Value::String(admission.context.target_id.clone());
    reply
}

fn action_error_with_message(
    invocation: &Invocation,
    session_id: &str,
    code: &str,
    message: &str,
    mode: &ExecutionMode,
    started: Instant,
    sequence: u64,
) -> InvocationReply {
    action_error_with_optional_message(
        invocation,
        session_id,
        code,
        Some(message),
        Some(mode),
        started,
        Some(sequence),
    )
}

fn action_error_with_optional_message(
    invocation: &Invocation,
    session_id: &str,
    code: &str,
    message: Option<&str>,
    mode: Option<&ExecutionMode>,
    started: Instant,
    sequence: Option<u64>,
) -> InvocationReply {
    let (error, exit_code) = operational_error(code, message);
    let possible = matches!(error.effects.as_str(), "possible" | "confirmed");
    let outcome = if possible {
        "uncertain"
    } else {
        "not_performed"
    };
    let delivery = if error.effects == "confirmed" {
        "backend_confirmed"
    } else if possible {
        "unknown"
    } else {
        "not_dispatched"
    };
    let requested = mode.map(ExecutionMode::as_str).unwrap_or("background");
    InvocationReply {
        value: json!({
            "schema": "manuvra/action-result@1",
            "protocol_version": "1.0",
            "registry_version": "1.0.0",
            "request_id": invocation.request_id,
            "command": invocation.command,
            "session_id": if session_id.starts_with("s_") { session_id } else { "s_unknown" },
            "target_id": null,
            "action_sequence": sequence,
            "outcome": outcome,
            "delivery": delivery,
            "requested_mode": requested,
            "effective_mode": mode.map(ExecutionMode::as_str),
            "effect_verification": "not_asserted",
            "observation": {
                "status": if code == "stabilization_timeout" { "timed_out" } else if code == "interrupted" { "interrupted" } else { "not_attempted" },
                "screenshot_path": null,
                "frame_token": null,
                "action_sequence_before": sequence,
                "action_sequence_after": sequence,
            },
            "timing_ms": timing(millis(started.elapsed())),
            "warnings": [],
            "error": error,
            "manifest_path": null,
        }),
        exit_code,
    }
}

fn timing(total: u64) -> Value {
    json!({
        "queue": 0, "preflight": 0, "dispatch": 0,
        "stabilize": 0, "capture": 0, "artifact": 0, "total": total,
    })
}

fn adapter_timing(total: u64, artifact_ms: u64, timing: &crate::model::AdapterTiming) -> Value {
    json!({
        "queue": 0,
        "preflight": timing.preflight_ms,
        "dispatch": timing.dispatch_ms,
        "stabilize": timing.stabilize_ms,
        "capture": timing.capture_ms,
        "artifact": artifact_ms,
        "total": total,
    })
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn add_usage_warning(value: &mut Value) {
    if let Some(warnings) = value.get_mut("warnings").and_then(Value::as_array_mut) {
        if !warnings
            .iter()
            .any(|warning| warning == "usage_not_recorded")
        {
            warnings.push(Value::String("usage_not_recorded".to_owned()));
        }
    } else if let Some(warning) = value.get_mut("warning") {
        *warning = Value::String("usage_not_recorded".to_owned());
    }
}
