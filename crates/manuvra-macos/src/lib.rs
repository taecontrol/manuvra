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
        let mut discovery = self.discovery.lock().expect("macOS discovery state");
        discovery
            .validated_record(&context.target_id, context.target_generation)
            .ok_or_else(|| AdapterError {
                code: "target_stale".to_owned(),
                message: Some("macOS window identity or generation changed".to_owned()),
                details: None,
            })
    }

    fn permission_check(
        &self,
        context: &AdapterContext,
        operation: &AdapterOperation,
    ) -> Result<(), AdapterError> {
        let permissions = permissions::PermissionSnapshot::current();
        permissions
            .missing_for(&operation.command, context.mode.as_str() == "foreground")
            .map(permission_error)
            .map_or(Ok(()), Err)
    }
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
        if std::time::Instant::now() >= deadline {
            return Err(ax::adapter_error(
                "timed_out",
                "deadline elapsed before macOS target discovery",
            ));
        }
        let targets = self.targets();
        if std::time::Instant::now() >= deadline {
            return Err(ax::adapter_error(
                "timed_out",
                "deadline elapsed during macOS target discovery",
            ));
        }
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
        if std::time::Instant::now() >= deadline {
            return Err(ax::adapter_error(
                "timed_out",
                "deadline elapsed before macOS session initialization",
            ));
        }
        self.evidence
            .lock()
            .expect("macOS evidence state")
            .opened(session);
        if let Some(record) = self
            .discovery
            .lock()
            .expect("macOS discovery state")
            .record(&session.target_id, session.target_generation)
        {
            self.workers
                .lock()
                .expect("macOS worker pool")
                .session_opened(&record, &session.session_id);
        }
        if std::time::Instant::now() >= deadline {
            self.session_closed(session);
            return Err(ax::adapter_error(
                "timed_out",
                "deadline elapsed during macOS session initialization",
            ));
        }
        Ok(())
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
        let authority = self
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
        let worker = self
            .workers
            .lock()
            .expect("macOS worker pool")
            .handle(&record);
        worker.prepare(record, context, operation, cancellation)
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
        let record = match self.record(context) {
            Ok(record) => record,
            Err(error) => return ax::rejected_error(error),
        };
        if operation.command == "observe.evidence" {
            return self
                .evidence
                .lock()
                .expect("macOS evidence state")
                .reply(&record, context, operation);
        }
        if operation.command == "observe.screenshot" {
            let reply = capture::screenshot(&record, context, &cancellation);
            self.evidence
                .lock()
                .expect("macOS evidence state")
                .record(context, operation, &reply);
            self.remember_frame(context, &reply);
            return reply;
        }
        let worker = self
            .workers
            .lock()
            .expect("macOS worker pool")
            .handle(&record);
        let reply = worker.dispatch(record, context, operation, cancellation);
        self.evidence
            .lock()
            .expect("macOS evidence state")
            .record(context, operation, &reply);
        self.remember_frame(context, &reply);
        reply
    }
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
