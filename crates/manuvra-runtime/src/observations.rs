use crate::artifacts::ArtifactWrite;
use crate::model::{
    AdapterContext, AdapterDelivery, AdapterOperation, AdapterReply, ExecutionMode, SessionState,
    TargetAdapter,
};
use crate::validation::Input;
use crate::{InvocationReply, Runtime};
use manuvra_protocol::{Invocation, sha256_hex};
use serde_json::{Value, json};
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

struct ObservationAdmission {
    session_id: String,
    target_id: String,
    target_generation: u64,
    directory: PathBuf,
    sequence_before: u64,
    mode: ExecutionMode,
    reference_namespace: String,
    reference_epoch: u64,
    frame_token: Option<String>,
    cancellation: Arc<AtomicBool>,
    adapter: Arc<dyn TargetAdapter>,
}

impl Runtime {
    pub(crate) fn observe(&self, invocation: &Invocation, started: Instant) -> InvocationReply {
        let input = match observation_input(invocation) {
            Ok(input) => input,
            Err(reply) => return reply,
        };
        let session_id = match input.string("session_id") {
            Ok(session_id) => session_id,
            Err(message) => return InvocationReply::error("invalid_request", Some(&message)),
        };
        let admission = match self.admit_observation(invocation, session_id, started) {
            Ok(admission) => admission,
            Err(reply) => return reply,
        };
        let reply = match invocation.command.as_str() {
            "observe.query" => self.observe_query(invocation, &input, &admission, started),
            "observe.screenshot" => self.observe_screenshot(invocation, &admission, started),
            "observe.tree" => self.observe_tree(invocation, &admission, started),
            "observe.evidence" => self.observe_evidence(invocation, &input, &admission, started),
            _ => InvocationReply::error("unknown_command", None),
        };
        self.finish_observation(invocation, &admission);
        reply
    }

    fn admit_observation(
        &self,
        invocation: &Invocation,
        session_id: &str,
        started: Instant,
    ) -> Result<ObservationAdmission, InvocationReply> {
        let mut state = self.state.lock().expect("runtime state");
        let session = state
            .sessions
            .get(session_id)
            .cloned()
            .ok_or_else(|| InvocationReply::error("session_not_found", None))?;
        if session.state == SessionState::Closing {
            return Err(InvocationReply::error("session_busy", None));
        }
        let (target, adapter) = self
            .target_with_adapter_until(
                &session.target_id,
                started + Duration::from_millis(invocation.deadline_ms),
            )
            .map_err(|error| InvocationReply::error(&error.code, error.message.as_deref()))?
            .ok_or_else(|| InvocationReply::error("target_not_found", None))?;
        if target.generation != session.target_generation {
            return Err(InvocationReply::error("target_stale", None));
        }
        let capability = observation_capability(&invocation.command);
        if !target
            .capabilities
            .iter()
            .any(|candidate| candidate == capability)
        {
            return Err(InvocationReply::error("capability_unavailable", None));
        }
        state
            .sessions
            .get_mut(session_id)
            .expect("session exists")
            .in_flight += 1;
        let cancellation = Arc::new(AtomicBool::new(false));
        state
            .cancellations
            .insert(invocation.request_id.clone(), cancellation.clone());
        state
            .cancellation_sessions
            .insert(invocation.request_id.clone(), session_id.to_owned());
        Ok(ObservationAdmission {
            session_id: session.id,
            target_id: session.target_id.clone(),
            target_generation: session.target_generation,
            directory: session.directory,
            sequence_before: *state.action_sequences.get(&session.target_id).unwrap_or(&0),
            mode: session.mode,
            reference_namespace: session.reference_namespace,
            reference_epoch: session.reference_epoch,
            frame_token: session.frame_token,
            cancellation,
            adapter,
        })
    }

    fn observe_query(
        &self,
        invocation: &Invocation,
        input: &Input<'_>,
        admission: &ObservationAdmission,
        started: Instant,
    ) -> InvocationReply {
        match input.value("semantic") {
            Ok(value) if value.is_object() => {}
            _ => {
                return InvocationReply::error(
                    "invalid_request",
                    Some("semantic locator is required"),
                );
            }
        }
        let epoch = self.next_reference_epoch(&admission.session_id);
        let reply = match self.invoke_observation(invocation, admission, epoch, started) {
            Ok(reply) => reply,
            Err(reply) => return reply,
        };
        let matches = reply
            .response
            .get("matches")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if matches.is_empty() {
            return InvocationReply::error("element_not_found", None);
        }
        let public_matches = matches
            .iter()
            .map(|entry| {
                let backend = entry
                    .get("backend_id")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                json!({
                    "ref": element_ref(admission, epoch, backend),
                    "role": entry.get("role").cloned().unwrap_or(Value::Null),
                    "name": entry.get("name").cloned().unwrap_or(Value::Null),
                    "text": entry.get("text").cloned().unwrap_or(Value::Null),
                    "identifier": entry.get("identifier").cloned().unwrap_or(Value::Null),
                })
            })
            .collect::<Vec<_>>();
        let overflow = reply
            .response
            .get("overflow")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let overflow_path = if overflow.is_empty() {
            None
        } else {
            let bytes = serde_json::to_vec(&overflow).expect("query overflow");
            match self.artifacts.publish(
                &admission.directory,
                ArtifactWrite {
                    kind: "query_overflow",
                    extension: "json",
                    media_type: "application/json",
                    bytes: &bytes,
                    request_id: &invocation.request_id,
                    action_sequence: admission.sequence_before,
                },
            ) {
                Ok(published) => Some(published.path),
                Err(error) => {
                    return InvocationReply::error("observation_failed", Some(&error.to_string()));
                }
            }
        };
        let sequence_after = self.action_sequence(&admission.target_id);
        InvocationReply::success(json!({
            "session_id": admission.session_id,
            "target_id": admission.target_id,
            "ref_epoch": format!("r_{epoch}"),
            "matches": public_matches,
            "overflow_path": overflow_path,
            "action_sequence_before": admission.sequence_before,
            "action_sequence_after": sequence_after,
            "observation_status": observation_status(admission.sequence_before, sequence_after),
        }))
    }

    fn observe_screenshot(
        &self,
        invocation: &Invocation,
        admission: &ObservationAdmission,
        started: Instant,
    ) -> InvocationReply {
        let reply = match self.invoke_observation(
            invocation,
            admission,
            admission.reference_epoch,
            started,
        ) {
            Ok(reply) => reply,
            Err(reply) => return reply,
        };
        let Some(bytes) = reply.screenshot else {
            return InvocationReply::error(
                "observation_failed",
                Some("adapter returned no screenshot"),
            );
        };
        let sequence_after = self.action_sequence(&admission.target_id);
        let published = match self.artifacts.publish(
            &admission.directory,
            ArtifactWrite {
                kind: "screenshot",
                extension: "png",
                media_type: "image/png",
                bytes: &bytes,
                request_id: &invocation.request_id,
                action_sequence: sequence_after,
            },
        ) {
            Ok(published) => published,
            Err(error) => {
                return InvocationReply::error("observation_failed", Some(&error.to_string()));
            }
        };
        let signature = reply.frame_signature.as_deref().unwrap_or("unbound");
        let frame_token = format!(
            "f_{}_{}_{}_{}",
            admission.session_id, admission.target_generation, sequence_after, signature
        );
        self.set_frame_token(&admission.session_id, frame_token.clone());
        InvocationReply::success(json!({
            "session_id": admission.session_id,
            "target_id": admission.target_id,
            "screenshot_path": published.path,
            "frame_token": frame_token,
            "width": reply.screenshot_width.unwrap_or(1),
            "height": reply.screenshot_height.unwrap_or(1),
            "sha256": published.sha256,
            "action_sequence_before": admission.sequence_before,
            "action_sequence_after": sequence_after,
            "observation_status": observation_status(admission.sequence_before, sequence_after),
            "manifest_path": published.manifest_path,
        }))
    }

    fn observe_tree(
        &self,
        invocation: &Invocation,
        admission: &ObservationAdmission,
        started: Instant,
    ) -> InvocationReply {
        let epoch = self.next_reference_epoch(&admission.session_id);
        let reply = match self.invoke_observation(invocation, admission, epoch, started) {
            Ok(reply) => reply,
            Err(reply) => return reply,
        };
        let sequence_after = self.action_sequence(&admission.target_id);
        let tree = reply.response.get("tree").cloned().unwrap_or_else(|| {
            json!({
                "complete": true, "target_id": admission.target_id, "nodes": []
            })
        });
        let node_count = reply
            .response
            .get("node_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if node_count == 0 {
            return InvocationReply::error(
                "observation_failed",
                Some("adapter returned an empty tree"),
            );
        }
        let bytes = serde_json::to_vec(&tree).expect("fake tree");
        let published = match self.artifacts.publish(
            &admission.directory,
            ArtifactWrite {
                kind: "accessibility_tree",
                extension: "json",
                media_type: "application/json",
                bytes: &bytes,
                request_id: &invocation.request_id,
                action_sequence: sequence_after,
            },
        ) {
            Ok(published) => published,
            Err(error) => {
                return InvocationReply::error("observation_failed", Some(&error.to_string()));
            }
        };
        InvocationReply::success(json!({
            "schema": "manuvra/complete-tree-result@1",
            "request_id": invocation.request_id,
            "session_id": admission.session_id,
            "target_id": admission.target_id,
            "complete": true,
            "tree_path": published.path,
            "sha256": published.sha256,
            "node_count": node_count,
            "ref_epoch": format!("r_{epoch}"),
            "action_sequence_before": admission.sequence_before,
            "action_sequence_after": sequence_after,
            "observation_status": observation_status(admission.sequence_before, sequence_after),
            "manifest_path": published.manifest_path,
        }))
    }

    fn observe_evidence(
        &self,
        invocation: &Invocation,
        input: &Input<'_>,
        admission: &ObservationAdmission,
        started: Instant,
    ) -> InvocationReply {
        let kind = match input.string("kind") {
            Ok(kind @ ("logs" | "events" | "diagnostics" | "timings" | "manifest")) => kind,
            _ => return InvocationReply::error("invalid_request", Some("invalid evidence kind")),
        };
        if kind == "manifest" {
            return manifest_pointer(self, admission);
        }
        let reply = match self.invoke_observation(
            invocation,
            admission,
            admission.reference_epoch,
            started,
        ) {
            Ok(reply) => reply,
            Err(reply) => return reply,
        };
        let sequence_after = self.action_sequence(&admission.target_id);
        if reply.response.get("complete").and_then(Value::as_bool) == Some(false) {
            return InvocationReply::error(
                "observation_failed",
                Some("adapter evidence journal overflowed; no partial artifact was published"),
            );
        }
        let (extension, media_type, bytes) = match reply.artifact {
            Some(artifact) => (artifact.extension, artifact.media_type, artifact.bytes),
            None => (
                "json".to_owned(),
                "application/json".to_owned(),
                serde_json::to_vec(&reply.response).expect("adapter evidence"),
            ),
        };
        match self.artifacts.publish(
            &admission.directory,
            ArtifactWrite {
                kind,
                extension: &extension,
                media_type: &media_type,
                bytes: &bytes,
                request_id: &invocation.request_id,
                action_sequence: sequence_after,
            },
        ) {
            Ok(published) => InvocationReply::success(json!({
                "kind": kind,
                "path": published.path,
                "sha256": published.sha256,
                "manifest_path": published.manifest_path,
            })),
            Err(error) => InvocationReply::error("observation_failed", Some(&error.to_string())),
        }
    }

    fn finish_observation(&self, invocation: &Invocation, admission: &ObservationAdmission) {
        let mut state = self.state.lock().expect("runtime state");
        state.cancellations.remove(&invocation.request_id);
        state.cancellation_sessions.remove(&invocation.request_id);
        if let Some(session) = state.sessions.get_mut(&admission.session_id) {
            session.in_flight = session.in_flight.saturating_sub(1);
        }
    }

    fn invoke_observation(
        &self,
        invocation: &Invocation,
        admission: &ObservationAdmission,
        reference_epoch: u64,
        started: Instant,
    ) -> Result<AdapterReply, InvocationReply> {
        let context = AdapterContext {
            session_id: admission.session_id.clone(),
            target_id: admission.target_id.clone(),
            target_generation: admission.target_generation,
            action_sequence: admission.sequence_before,
            reference_namespace: admission.reference_namespace.clone(),
            reference_epoch,
            frame_token: admission.frame_token.clone(),
            mode: admission.mode.clone(),
            deadline: started + Duration::from_millis(invocation.deadline_ms),
        };
        let reply = catch_unwind(AssertUnwindSafe(|| {
            admission.adapter.invoke(
                &context,
                &AdapterOperation::new(invocation.command.clone(), invocation.input.clone()),
                admission.cancellation.clone(),
            )
        }))
        .map_err(|_| InvocationReply::error("internal_error", None))?;
        match reply.delivery {
            AdapterDelivery::Confirmed => Ok(reply),
            AdapterDelivery::Rejected | AdapterDelivery::Unknown => {
                let code = reply
                    .error
                    .as_ref()
                    .map(|error| error.code.as_str())
                    .unwrap_or("observation_failed");
                Err(InvocationReply::error(
                    code,
                    reply
                        .error
                        .as_ref()
                        .and_then(|error| error.message.as_deref()),
                ))
            }
        }
    }

    fn next_reference_epoch(&self, session_id: &str) -> u64 {
        let mut state = self.state.lock().expect("runtime state");
        let session = state
            .sessions
            .get_mut(session_id)
            .expect("admitted session");
        session.reference_epoch += 1;
        session.reference_epoch
    }

    fn set_frame_token(&self, session_id: &str, frame_token: String) {
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

    fn action_sequence(&self, target_id: &str) -> u64 {
        *self
            .state
            .lock()
            .expect("runtime state")
            .action_sequences
            .get(target_id)
            .unwrap_or(&0)
    }
}

fn observation_input(invocation: &Invocation) -> Result<Input<'_>, InvocationReply> {
    let allowed = match invocation.command.as_str() {
        "observe.query" => &["session_id", "semantic", "limit"][..],
        "observe.screenshot" | "observe.tree" => &["session_id"],
        "observe.evidence" => &["session_id", "kind"],
        _ => &[],
    };
    Input::new(&invocation.input, allowed)
        .map_err(|message| InvocationReply::error("invalid_request", Some(&message)))
}

fn observation_capability(command: &str) -> &'static str {
    match command {
        "observe.query" => "observation.query",
        "observe.screenshot" => "observation.screenshot",
        "observe.tree" => "observation.tree",
        "observe.evidence" => "observation.evidence",
        _ => "unknown",
    }
}

fn observation_status(before: u64, after: u64) -> &'static str {
    if before == after {
        "stable"
    } else {
        "concurrent"
    }
}

fn element_ref(admission: &ObservationAdmission, epoch: u64, backend: &str) -> String {
    format!("e_{}_{}_{}", admission.reference_namespace, epoch, backend)
}

fn manifest_pointer(runtime: &Runtime, admission: &ObservationAdmission) -> InvocationReply {
    let path = runtime.artifacts.manifest_path(&admission.directory);
    match fs::read(&path) {
        Ok(bytes) => InvocationReply::success(json!({
            "kind": "manifest",
            "path": path,
            "sha256": sha256_hex(&bytes),
            "manifest_path": runtime.artifacts.manifest_path(&admission.directory),
        })),
        Err(error) => InvocationReply::error("observation_failed", Some(&error.to_string())),
    }
}
