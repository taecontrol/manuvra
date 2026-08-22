#[cfg(target_os = "macos")]
mod ax;
#[cfg(target_os = "macos")]
mod capture;
#[cfg(target_os = "macos")]
mod discovery;
#[cfg(target_os = "macos")]
mod evidence;
#[cfg(target_os = "macos")]
mod foreground;
#[cfg(target_os = "macos")]
mod observer;
#[cfg(target_os = "macos")]
mod oracle;
#[cfg(target_os = "macos")]
mod permissions;
#[cfg(target_os = "macos")]
mod seam;
#[cfg(all(test, target_os = "macos"))]
mod test_oracles;
#[cfg(target_os = "macos")]
mod worker;

#[cfg(target_os = "macos")]
use manuvra_runtime::{
    AdapterContext, AdapterError, AdapterOperation, AdapterReply, AdapterSession, TargetAdapter,
    TargetDescriptor,
};
#[cfg(target_os = "macos")]
use serde_json::{Value, json};
#[cfg(target_os = "macos")]
use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::sync::atomic::AtomicBool;
#[cfg(target_os = "macos")]
use std::sync::{Arc, Mutex};

#[cfg(target_os = "macos")]
pub struct MacosAdapter {
    discovery: Mutex<discovery::DiscoveryState>,
    evidence: Mutex<evidence::EvidenceState>,
    workers: Mutex<worker::WorkerPool>,
    frames: Mutex<HashMap<String, capture::FrameAuthority>>,
}

#[cfg(target_os = "macos")]
impl MacosAdapter {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            discovery: Mutex::new(discovery::DiscoveryState::new()),
            evidence: Mutex::new(evidence::EvidenceState::default()),
            workers: Mutex::new(worker::WorkerPool::new()),
            frames: Mutex::new(HashMap::new()),
        })
    }

    fn record(&self, context: &AdapterContext) -> Result<discovery::WindowRecord, AdapterError> {
        validated_window(
            &mut self.discovery.lock().expect("macOS discovery state"),
            context,
        )
    }

    fn permission_check(
        &self,
        context: &AdapterContext,
        operation: &AdapterOperation,
    ) -> Result<(), AdapterError> {
        require_operation_permission(context, operation)
    }
}

fn validated_window(
    discovery: &mut discovery::DiscoveryState,
    context: &AdapterContext,
) -> Result<discovery::WindowRecord, AdapterError> {
    discovery
        .validated_record(&context.target_id, context.target_generation)
        .ok_or_else(|| AdapterError {
            code: "target_stale".to_owned(),
            message: Some("macOS window identity or generation changed".to_owned()),
            details: None,
        })
}

fn require_operation_permission(
    context: &AdapterContext,
    operation: &AdapterOperation,
) -> Result<(), AdapterError> {
    permissions::PermissionSnapshot::current()
        .missing_for(&operation.command, context.mode.as_str() == "foreground")
        .map(permission_error)
        .map_or(Ok(()), Err)
}

pub(crate) fn permission_error(permission: permissions::MissingPermission) -> AdapterError {
    let (permission, recovery) = match permission {
        permissions::MissingPermission::Accessibility => (
            "Accessibility",
            "System Settings > Privacy & Security > Accessibility",
        ),
        permissions::MissingPermission::ScreenRecording => (
            "Screen Recording",
            "System Settings > Privacy & Security > Screen & System Audio Recording",
        ),
        permissions::MissingPermission::PostEvent => (
            "Post Event",
            "System Settings > Privacy & Security > Accessibility",
        ),
    };
    AdapterError {
        code: "permission_required".to_owned(),
        message: Some(format!(
            "{permission} permission is required by manuvra-daemon; enable it in {recovery}, then run manuvra doctor"
        )),
        details: Some(json!({
            "permission": permission,
            "recovery": recovery,
            "prompts_triggered": false,
        })),
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn native_adapter_seams_keep_discovery_permission_and_background_results() {
        let adapter = MacosAdapter::new().unwrap();
        let expired = std::time::Instant::now();
        assert_eq!(
            adapter.targets_until(expired).unwrap_err().code,
            "timed_out"
        );
        let session = AdapterSession {
            session_id: "s_macos".to_owned(),
            target_id: "macos_missing".to_owned(),
            target_generation: 1,
        };
        assert_eq!(
            adapter.session_opened(&session, expired).unwrap_err().code,
            "timed_out"
        );
        adapter
            .session_opened(
                &session,
                std::time::Instant::now() + std::time::Duration::from_secs(1),
            )
            .unwrap();
        adapter.session_closed(&session);
        let context = AdapterContext {
            session_id: session.session_id.clone(),
            target_id: session.target_id.clone(),
            target_generation: 1,
            action_sequence: 1,
            reference_namespace: "n".to_owned(),
            reference_epoch: 1,
            frame_token: None,
            mode: manuvra_runtime::ExecutionMode::Background,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(1),
        };
        let click = AdapterOperation::new(
            "action.click".to_owned(),
            json!({"locator": {"kind": "point", "x": 1, "y": 1}}),
        );
        let reply = adapter.invoke(&context, &click, Arc::new(AtomicBool::new(false)));
        assert!(matches!(
            reply.error.as_ref().unwrap().code.as_str(),
            "target_stale" | "permission_required" | "frame_stale"
        ));
        let diagnostics = adapter.diagnostics();
        assert_eq!(diagnostics["kind"], "macos");
        assert!(diagnostics.get("permissions").is_some());
        let _ = adapter.targets();
        assert_eq!(
            adapter
                .prepare(&context, &click, Arc::new(AtomicBool::new(false)))
                .unwrap_err()
                .code
                .as_str(),
            reply.error.as_ref().unwrap().code.as_str()
        );
        assert_eq!(
            require_deadline(expired, "deadline elapsed before macOS target discovery")
                .unwrap_err()
                .code,
            "timed_out"
        );
    }

    #[test]
    fn every_missing_permission_maps_to_an_actionable_recovery() {
        let accessibility = permission_error(permissions::MissingPermission::Accessibility);
        let screen = permission_error(permissions::MissingPermission::ScreenRecording);
        let post = permission_error(permissions::MissingPermission::PostEvent);

        assert_eq!(accessibility.code, "permission_required");
        assert!(
            accessibility
                .message
                .as_deref()
                .unwrap()
                .contains("Accessibility")
        );
        assert!(
            screen
                .message
                .as_deref()
                .unwrap()
                .contains("Screen & System Audio Recording")
        );
        assert!(post.message.as_deref().unwrap().contains("Post Event"));
    }
}

#[cfg(target_os = "macos")]
impl TargetAdapter for MacosAdapter {
    fn targets(&self) -> Vec<TargetDescriptor> {
        let targets = self
            .discovery
            .lock()
            .expect("macOS discovery state")
            .refresh_native();
        self.workers
            .lock()
            .expect("macOS worker pool")
            .retain_present(&targets);
        targets
    }

    fn targets_until(
        &self,
        deadline: std::time::Instant,
    ) -> Result<Vec<TargetDescriptor>, AdapterError> {
        require_deadline(deadline, "deadline elapsed before macOS target discovery")?;
        let targets = self.targets();
        require_deadline(deadline, "deadline elapsed during macOS target discovery")?;
        Ok(targets)
    }

    fn diagnostics(&self) -> Value {
        let mut discovery = self.discovery.lock().expect("macOS discovery state");
        let targets = discovery.refresh_native();
        json!({
            "kind": "macos",
            "permissions": permissions::PermissionSnapshot::current().diagnostics(),
            "targets": targets.len(),
            "discovery_error": discovery.last_error,
        })
    }

    fn setup_permissions(
        &self,
        deadline: std::time::Instant,
    ) -> Option<Result<Value, AdapterError>> {
        Some(
            permissions::setup_permissions(deadline).map_err(|error| match error {
                permissions::SetupPermissionsError::Deadline => ax::adapter_error(
                    "timed_out",
                    "deadline elapsed during macOS permission setup",
                ),
                permissions::SetupPermissionsError::Settings(permission) => ax::adapter_error(
                    "internal_error",
                    &format!("failed to open System Settings for {permission}"),
                ),
            }),
        )
    }

    fn session_opened(
        &self,
        session: &AdapterSession,
        deadline: std::time::Instant,
    ) -> Result<(), AdapterError> {
        require_deadline(
            deadline,
            "deadline elapsed before macOS session initialization",
        )?;
        open_session_resources(self, session);
        finish_session_open(self, session, deadline)
    }

    fn session_closed(&self, session: &AdapterSession) {
        self.evidence
            .lock()
            .expect("macOS evidence state")
            .closed(session);
        self.frames
            .lock()
            .expect("macOS frame authority")
            .remove(&session.session_id);
        self.workers
            .lock()
            .expect("macOS worker pool")
            .session_closed(&session.target_id, &session.session_id);
    }

    fn prepare(
        &self,
        context: &AdapterContext,
        operation: &AdapterOperation,
        cancellation: Arc<AtomicBool>,
    ) -> Result<AdapterOperation, AdapterError> {
        let record = self.record(context)?;
        self.permission_check(context, operation)?;
        prepare_native(self, record, context, operation, cancellation)
    }

    fn invoke(
        &self,
        context: &AdapterContext,
        operation: &AdapterOperation,
        cancellation: Arc<AtomicBool>,
    ) -> AdapterReply {
        if let Err(error) = self.permission_check(context, operation) {
            return ax::rejected_error(error);
        }
        invoke_recorded(self, context, operation, cancellation)
    }
}

fn require_deadline(deadline: std::time::Instant, message: &str) -> Result<(), AdapterError> {
    (std::time::Instant::now() < deadline)
        .then_some(())
        .ok_or_else(|| ax::adapter_error("timed_out", message))
}

fn open_session_resources(adapter: &MacosAdapter, session: &AdapterSession) {
    adapter
        .evidence
        .lock()
        .expect("macOS evidence state")
        .opened(session);
    if let Some(record) = adapter
        .discovery
        .lock()
        .expect("macOS discovery state")
        .record(&session.target_id, session.target_generation)
    {
        adapter
            .workers
            .lock()
            .expect("macOS worker pool")
            .session_opened(&record, &session.session_id);
    }
}

fn finish_session_open(
    adapter: &MacosAdapter,
    session: &AdapterSession,
    deadline: std::time::Instant,
) -> Result<(), AdapterError> {
    if std::time::Instant::now() < deadline {
        return Ok(());
    }
    adapter.session_closed(session);
    Err(ax::adapter_error(
        "timed_out",
        "deadline elapsed during macOS session initialization",
    ))
}

fn prepare_native(
    adapter: &MacosAdapter,
    record: discovery::WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
) -> Result<AdapterOperation, AdapterError> {
    let authority = adapter
        .frames
        .lock()
        .expect("macOS frame authority")
        .get(&context.session_id)
        .cloned();
    let operation = capture::prepare_point(
        &record,
        context,
        operation,
        authority.as_ref(),
        &cancellation,
    )?;
    adapter
        .workers
        .lock()
        .expect("macOS worker pool")
        .handle(&record)
        .prepare(record, context, operation, cancellation)
}

fn invoke_recorded(
    adapter: &MacosAdapter,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
) -> AdapterReply {
    match adapter.record(context) {
        Ok(record) => invoke_native(adapter, record, context, operation, cancellation),
        Err(error) => ax::rejected_error(error),
    }
}

fn invoke_native(
    adapter: &MacosAdapter,
    record: discovery::WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
) -> AdapterReply {
    if operation.command == "observe.evidence" {
        return adapter
            .evidence
            .lock()
            .expect("macOS evidence state")
            .reply(&record, context, operation);
    }
    invoke_capture_or_worker(adapter, record, context, operation, cancellation)
}

fn invoke_capture_or_worker(
    adapter: &MacosAdapter,
    record: discovery::WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
) -> AdapterReply {
    if operation.command == "observe.screenshot" {
        invoke_screenshot(adapter, &record, context, operation, &cancellation)
    } else {
        invoke_worker(adapter, record, context, operation, cancellation)
    }
}

fn invoke_screenshot(
    adapter: &MacosAdapter,
    record: &discovery::WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: &AtomicBool,
) -> AdapterReply {
    let reply = capture::screenshot(record, context, cancellation);
    remember_native_result(adapter, context, operation, &reply);
    reply
}

fn invoke_worker(
    adapter: &MacosAdapter,
    record: discovery::WindowRecord,
    context: &AdapterContext,
    operation: &AdapterOperation,
    cancellation: Arc<AtomicBool>,
) -> AdapterReply {
    let worker = adapter
        .workers
        .lock()
        .expect("macOS worker pool")
        .handle(&record);
    let reply = worker.dispatch(record, context, operation, cancellation);
    remember_native_result(adapter, context, operation, &reply);
    reply
}

fn remember_native_result(
    adapter: &MacosAdapter,
    context: &AdapterContext,
    operation: &AdapterOperation,
    reply: &AdapterReply,
) {
    adapter
        .evidence
        .lock()
        .expect("macOS evidence state")
        .record(context, operation, reply);
    adapter.remember_frame(context, reply);
}

#[cfg(target_os = "macos")]
impl MacosAdapter {
    fn remember_frame(&self, context: &AdapterContext, reply: &AdapterReply) {
        if let Some(authority) = capture::authority(reply) {
            self.frames
                .lock()
                .expect("macOS frame authority")
                .insert(context.session_id.clone(), authority);
        }
    }
}
