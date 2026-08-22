use manuvra_protocol::{Invocation, encode_operational_line, validate_command_result};
use manuvra_runtime::fake::FakeAdapter;
use manuvra_runtime::{
    AdapterContext, AdapterDelivery, AdapterError, AdapterOperation, AdapterReply,
    InteractionModule, InvocationReply, Runtime, RuntimeConfig, TargetAdapter, TargetDescriptor,
};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, mpsc};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

struct Harness {
    _root: TempDir,
    config: std::path::PathBuf,
    runtime: Arc<Runtime>,
}

struct ReplacingAdapter {
    generation: Arc<AtomicU64>,
}

struct PredispatchRejectingAdapter;
struct DeadlineDiscoveryAdapter;
struct DelayedSessionAdapter {
    opened: AtomicUsize,
    closed: Arc<AtomicUsize>,
}
struct SetupTrackingAdapter {
    calls: Arc<AtomicUsize>,
    timeout: bool,
}
struct SequenceRaceAdapter {
    observation_started: Mutex<Option<mpsc::SyncSender<()>>>,
    observation_release: Barrier,
}

impl TargetAdapter for SequenceRaceAdapter {
    fn targets(&self) -> Vec<TargetDescriptor> {
        FakeAdapter.targets()
    }

    fn invoke(
        &self,
        context: &AdapterContext,
        operation: &AdapterOperation,
        cancellation: Arc<AtomicBool>,
    ) -> AdapterReply {
        if operation.command == "observe.screenshot"
            && let Some(started) = self.observation_started.lock().unwrap().take()
        {
            started.send(()).unwrap();
            self.observation_release.wait();
        }
        FakeAdapter.invoke(context, operation, cancellation)
    }
}

impl TargetAdapter for SetupTrackingAdapter {
    fn targets(&self) -> Vec<TargetDescriptor> {
        Vec::new()
    }

    fn setup_permissions(
        &self,
        _deadline: std::time::Instant,
    ) -> Option<Result<Value, AdapterError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.timeout {
            return Some(Err(AdapterError {
                code: "timed_out".to_owned(),
                message: Some("settings launch exhausted the command deadline".to_owned()),
                details: None,
            }));
        }
        let granted = json!({
            "before_granted": true,
            "prompt_requested": false,
            "settings_opened": false,
            "granted": true,
            "freshly_granted": false,
            "residual": false
        });
        Some(Ok(json!({
            "permissions": {
                "accessibility": granted,
                "screen_recording": granted,
                "post_event": granted
            }
        })))
    }

    fn invoke(
        &self,
        _context: &AdapterContext,
        _operation: &AdapterOperation,
        _cancellation: Arc<AtomicBool>,
    ) -> AdapterReply {
        panic!("setup adapter does not own session commands")
    }
}

impl TargetAdapter for DelayedSessionAdapter {
    fn targets(&self) -> Vec<TargetDescriptor> {
        vec![TargetDescriptor {
            target_id: "chrome_delayed_session".to_owned(),
            generation: 1,
            kind: "chrome".to_owned(),
            owner: "Chrome".to_owned(),
            title: Some("Delayed".to_owned()),
            capabilities: vec![],
        }]
    }

    fn session_opened(
        &self,
        _session: &manuvra_runtime::AdapterSession,
        deadline: std::time::Instant,
    ) -> Result<(), AdapterError> {
        if self.opened.fetch_add(1, Ordering::SeqCst) == 0 {
            thread::sleep(
                deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .saturating_add(Duration::from_millis(10)),
            );
            return Err(AdapterError {
                code: "timed_out".to_owned(),
                message: Some("delayed session initialization".to_owned()),
                details: None,
            });
        }
        Ok(())
    }

    fn session_closed(&self, _session: &manuvra_runtime::AdapterSession) {
        self.closed.fetch_add(1, Ordering::SeqCst);
    }

    fn invoke(
        &self,
        context: &AdapterContext,
        operation: &AdapterOperation,
        cancellation: Arc<AtomicBool>,
    ) -> AdapterReply {
        FakeAdapter.invoke(context, operation, cancellation)
    }
}

impl TargetAdapter for DeadlineDiscoveryAdapter {
    fn targets(&self) -> Vec<TargetDescriptor> {
        panic!("deadline-aware discovery seam was bypassed")
    }

    fn targets_until(
        &self,
        _deadline: std::time::Instant,
    ) -> Result<Vec<TargetDescriptor>, AdapterError> {
        Err(AdapterError {
            code: "timed_out".to_owned(),
            message: Some("discovery budget exhausted".to_owned()),
            details: None,
        })
    }

    fn invoke(
        &self,
        _context: &AdapterContext,
        _operation: &AdapterOperation,
        _cancellation: Arc<AtomicBool>,
    ) -> AdapterReply {
        panic!("timed-out discovery must not invoke an adapter")
    }
}

impl TargetAdapter for PredispatchRejectingAdapter {
    fn targets(&self) -> Vec<TargetDescriptor> {
        vec![TargetDescriptor {
            target_id: "macos_predispatch_reject".to_owned(),
            generation: 1,
            kind: "macos".to_owned(),
            owner: "Fixture".to_owned(),
            title: Some("Predispatch".to_owned()),
            capabilities: ["common.press", "observation.screenshot"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        }]
    }

    fn invoke(
        &self,
        _context: &AdapterContext,
        _operation: &AdapterOperation,
        _cancellation: Arc<AtomicBool>,
    ) -> AdapterReply {
        let mut reply = AdapterReply::confirmed(Value::Null, None);
        reply.delivery = AdapterDelivery::Rejected;
        reply.error = Some(AdapterError {
            code: "interrupted".to_owned(),
            message: Some("ownership was lost before input".to_owned()),
            details: None,
        });
        reply
    }
}

impl TargetAdapter for ReplacingAdapter {
    fn targets(&self) -> Vec<TargetDescriptor> {
        vec![TargetDescriptor {
            target_id: "chrome_replacing_1".to_owned(),
            generation: self.generation.load(Ordering::SeqCst),
            kind: "chrome".to_owned(),
            owner: "Chrome".to_owned(),
            title: Some("Replacing".to_owned()),
            capabilities: [
                "common.click",
                "observation.query",
                "observation.screenshot",
                "raw.cdp",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        }]
    }

    fn invoke(
        &self,
        context: &AdapterContext,
        operation: &AdapterOperation,
        cancellation: Arc<AtomicBool>,
    ) -> AdapterReply {
        FakeAdapter.invoke(context, operation, cancellation)
    }
}

impl Harness {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let temporary = root.path().join("tmp");
        let config = root.path().join("config");
        let runtime = runtime(&temporary, &config);
        Self {
            _root: root,
            config,
            runtime,
        }
    }

    fn invoke(&self, request_id: &str, command: &str, input: Value) -> InvocationReply {
        invoke(&self.runtime, request_id, command, input, 1_000)
    }

    fn open(&self, target_id: &str, role: &str) -> String {
        let reply = self.invoke(
            &format!("open-{target_id}-{role}-{}", unique()),
            "session.open",
            json!({"target_id": target_id, "role": role, "mode": "background", "lease_ttl_ms": 10000}),
        );
        assert_eq!(reply.exit_code, 0, "{}", reply.value);
        reply.value["session_id"].as_str().unwrap().to_owned()
    }
}

fn runtime(temporary: &Path, config: &Path) -> Arc<Runtime> {
    Arc::new(
        Runtime::new(
            RuntimeConfig {
                temporary_root: temporary.to_path_buf(),
                config_root: config.to_path_buf(),
            },
            vec![Arc::new(FakeAdapter)],
        )
        .unwrap(),
    )
}

#[test]
fn admitted_predispatch_rejection_is_schema_valid_and_not_performed() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(
        Runtime::new(
            RuntimeConfig {
                temporary_root: root.path().join("tmp"),
                config_root: root.path().join("config"),
            },
            vec![Arc::new(PredispatchRejectingAdapter)],
        )
        .unwrap(),
    );
    let opened = invoke(
        &runtime,
        "open-predispatch-reject",
        "session.open",
        json!({
            "target_id": "macos_predispatch_reject",
            "role": "actor",
            "mode": "foreground",
            "lease_ttl_ms": 10_000
        }),
        1_000,
    );
    let session = opened.value["session_id"].as_str().unwrap();
    let rejected = invoke(
        &runtime,
        "predispatch-reject",
        "action.press",
        json!({"session_id": session, "key": "Enter", "mode": "foreground"}),
        1_000,
    );
    assert_eq!(rejected.value["error"]["code"], "interrupted");
    assert_eq!(rejected.value["error"]["effects"], "none");
    assert_eq!(rejected.value["outcome"], "not_performed");
    assert_eq!(rejected.value["delivery"], "backend_rejected");
    assert_eq!(rejected.value["observation"]["status"], "not_attempted");
}

#[test]
fn target_discovery_uses_and_propagates_the_invocation_deadline_seam() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(
        Runtime::new(
            RuntimeConfig {
                temporary_root: root.path().join("tmp"),
                config_root: root.path().join("config"),
            },
            vec![Arc::new(DeadlineDiscoveryAdapter)],
        )
        .unwrap(),
    );
    let reply = invoke(
        &runtime,
        "deadline-discovery",
        "target.list",
        json!({"limit": 10}),
        100,
    );
    assert_eq!(error_code(&reply), Some("timed_out"));
}

#[test]
fn timed_out_session_initialization_rolls_back_session_lease_adapter_and_directory() {
    let root = tempfile::tempdir().unwrap();
    let temporary = root.path().join("tmp");
    let closed = Arc::new(AtomicUsize::new(0));
    let runtime = Arc::new(
        Runtime::new(
            RuntimeConfig {
                temporary_root: temporary.clone(),
                config_root: root.path().join("config"),
            },
            vec![Arc::new(DelayedSessionAdapter {
                opened: AtomicUsize::new(0),
                closed: closed.clone(),
            })],
        )
        .unwrap(),
    );
    let input = json!({
        "target_id": "chrome_delayed_session",
        "role": "actor",
        "mode": "background",
        "lease_ttl_ms": 10_000
    });
    let timed_out = invoke(
        &runtime,
        "delayed-open",
        "session.open",
        input.clone(),
        1_000,
    );
    assert_eq!(error_code(&timed_out), Some("timed_out"));
    assert_eq!(closed.load(Ordering::SeqCst), 1);
    assert_eq!(
        fs::read_dir(temporary.join("manuvra/sessions-v1"))
            .unwrap()
            .count(),
        0
    );

    let replacement = invoke(&runtime, "replacement-open", "session.open", input, 500);
    assert_eq!(replacement.exit_code, 0, "{}", replacement.value);
}

fn invoke(
    runtime: &Arc<Runtime>,
    request_id: &str,
    command: &str,
    input: Value,
    deadline_ms: u64,
) -> InvocationReply {
    let reply = runtime.invoke(Invocation::new(
        command,
        input,
        request_id.to_owned(),
        deadline_ms,
    ));
    if reply.exit_code == 0
        || reply.value.get("schema").and_then(Value::as_str) == Some("manuvra/action-result@1")
    {
        validate_command_result(command, &reply.value)
            .unwrap_or_else(|error| panic!("invalid {command} result: {error}; {}", reply.value));
    }
    reply
}

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn error_code(reply: &InvocationReply) -> Option<&str> {
    reply.value.get("error")?.get("code")?.as_str()
}

#[test]
fn target_list_exposes_presentation_owner_and_title_for_chrome_and_macos() {
    let harness = Harness::new();
    let reply = harness.invoke("targets-labels", "target.list", json!({"limit": 10}));
    let targets = reply.value["targets"].as_array().unwrap();
    assert_eq!(targets.len(), 2);

    let chrome = targets
        .iter()
        .find(|target| target["kind"] == "chrome")
        .expect("chrome target");
    let macos = targets
        .iter()
        .find(|target| target["kind"] == "macos")
        .expect("macos target");

    assert_eq!(chrome["owner"], "Chrome");
    assert_eq!(chrome["title"], "Fake Chrome");
    assert_eq!(macos["owner"], "Fake");
    assert_eq!(macos["title"], "Fake Target");
    for target in [chrome, macos] {
        assert!(target["owner"].is_string(), "{target}");
        assert!(
            target.get("title").is_some(),
            "title key must be present: {target}"
        );
    }
}

#[test]
fn lifecycle_observers_single_actor_export_and_cleanup() {
    let harness = Harness::new();
    let targets = harness.invoke("targets-1", "target.list", json!({"limit": 10}));
    assert_eq!(targets.value["targets"].as_array().unwrap().len(), 2);

    let actor = harness.open("chrome_fake_1", "actor");
    let observer_one = harness.open("chrome_fake_1", "observer");
    let observer_two = harness.open("chrome_fake_1", "observer");
    let blocked = harness.invoke(
        "second-actor",
        "session.open",
        json!({"target_id": "chrome_fake_1", "role": "actor"}),
    );
    assert_eq!(error_code(&blocked), Some("actor_lease_held"));

    let screenshot = harness.invoke(
        "screenshot-1",
        "observe.screenshot",
        json!({"session_id": actor}),
    );
    let screenshot_path = screenshot.value["screenshot_path"].as_str().unwrap();
    let session_directory = Path::new(screenshot_path).parent().unwrap().to_path_buf();
    assert!(Path::new(screenshot_path).is_file());

    let click = harness.invoke(
        "click-1",
        "action.click",
        json!({
            "session_id": actor,
            "locator": {"kind": "semantic", "role": "button", "name": "Save"}
        }),
    );
    assert_eq!(click.value["outcome"], "observed");
    assert!(encode_operational_line(&click.value).unwrap().len() <= 4096);

    let destination = harness._root.path().join("export");
    let export = harness.invoke(
        "export-1",
        "artifact.export",
        json!({"session_id": actor, "all": true, "destination": destination}),
    );
    assert_eq!(export.value["verified"], true);
    assert!(destination.join("manifest.json").is_file());
    let exported_manifest: Value =
        serde_json::from_slice(&fs::read(destination.join("manifest.json")).unwrap()).unwrap();
    let idempotent = harness.invoke(
        "export-idempotent",
        "artifact.export",
        json!({"session_id": actor, "all": true, "destination": destination}),
    );
    assert_eq!(idempotent.value["verified"], true);
    let first_export = PathBuf::from(
        exported_manifest["artifacts"][0]["absolute_path"]
            .as_str()
            .unwrap(),
    );
    let original_export = fs::read(&first_export).unwrap();
    fs::write(&first_export, b"different").unwrap();
    let conflict = harness.invoke(
        "export-conflict",
        "artifact.export",
        json!({"session_id": actor, "all": true, "destination": destination}),
    );
    assert_eq!(error_code(&conflict), Some("export_failed"));
    assert_eq!(fs::read(&first_export).unwrap(), b"different");
    fs::write(&first_export, original_export).unwrap();

    let close = harness.invoke("close-1", "session.close", json!({"session_id": actor}));
    assert_eq!(close.value["artifacts_removed"], true);
    assert!(!session_directory.exists());
    assert!(destination.join("manifest.json").exists());
    let durable_manifest: Value =
        serde_json::from_slice(&fs::read(destination.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(
        durable_manifest["schema"],
        "manuvra/exported-artifact-manifest@1"
    );
    assert_eq!(durable_manifest["lifetime"], "caller_owned");
    assert_eq!(
        Path::new(durable_manifest["session_directory"].as_str().unwrap()),
        fs::canonicalize(&destination).unwrap()
    );
    for artifact in durable_manifest["artifacts"].as_array().unwrap() {
        assert_eq!(artifact["lifetime"], "caller_owned");
        let path = PathBuf::from(artifact["absolute_path"].as_str().unwrap());
        assert!(path.is_file());
        assert!(path.starts_with(fs::canonicalize(&destination).unwrap()));
        assert_eq!(
            manuvra_protocol::sha256_hex(&fs::read(path).unwrap()),
            artifact["sha256"].as_str().unwrap()
        );
    }
    assert_eq!(
        harness
            .invoke(
                "close-o1",
                "session.close",
                json!({"session_id": observer_one})
            )
            .exit_code,
        0
    );
    assert_eq!(
        harness
            .invoke(
                "close-o2",
                "session.close",
                json!({"session_id": observer_two})
            )
            .exit_code,
        0
    );
}

#[test]
fn racing_actor_opens_publish_exactly_one_session_and_lease() {
    let harness = Harness::new();
    let barrier = Arc::new(Barrier::new(3));
    let workers = ["race-actor-one", "race-actor-two"].map(|request_id| {
        let runtime = harness.runtime.clone();
        let barrier = barrier.clone();
        thread::spawn(move || {
            barrier.wait();
            invoke(
                &runtime,
                request_id,
                "session.open",
                json!({"target_id": "chrome_fake_1", "role": "actor"}),
                1_000,
            )
        })
    });
    barrier.wait();
    let replies = workers.map(|worker| worker.join().unwrap());
    assert_eq!(
        replies.iter().filter(|reply| reply.exit_code == 0).count(),
        1
    );
    assert_eq!(
        replies
            .iter()
            .filter(|reply| error_code(reply) == Some("actor_lease_held"))
            .count(),
        1
    );

    let sessions_root = harness._root.path().join("tmp/manuvra/sessions-v1");
    assert_eq!(fs::read_dir(&sessions_root).unwrap().count(), 1);
    let winner = replies
        .iter()
        .find(|reply| reply.exit_code == 0)
        .unwrap()
        .value["session_id"]
        .as_str()
        .unwrap();
    assert_eq!(
        harness
            .invoke(
                "close-race-winner",
                "session.close",
                json!({"session_id": winner})
            )
            .exit_code,
        0
    );
}

#[test]
fn target_generation_replacement_invalidates_old_session_and_frees_new_lease() {
    let root = tempfile::tempdir().unwrap();
    let generation = Arc::new(AtomicU64::new(1));
    let runtime = Arc::new(
        Runtime::new(
            RuntimeConfig {
                temporary_root: root.path().join("tmp"),
                config_root: root.path().join("config"),
            },
            vec![Arc::new(ReplacingAdapter {
                generation: generation.clone(),
            })],
        )
        .unwrap(),
    );
    let first = invoke(
        &runtime,
        "generation-one-open",
        "session.open",
        json!({"target_id": "chrome_replacing_1", "role": "actor"}),
        1_000,
    );
    let first_session = first.value["session_id"].as_str().unwrap();

    generation.store(2, Ordering::SeqCst);
    let stale = invoke(
        &runtime,
        "generation-one-action",
        "action.click",
        json!({"session_id": first_session, "locator": {"kind": "semantic", "name": "Old"}}),
        1_000,
    );
    assert_eq!(error_code(&stale), Some("target_stale"), "{}", stale.value);
    let stale_lease = invoke(
        &runtime,
        "generation-one-renew",
        "lease.manage",
        json!({"session_id": first_session, "action": "renew"}),
        1_000,
    );
    assert_eq!(error_code(&stale_lease), Some("target_stale"));

    let replacement = invoke(
        &runtime,
        "generation-two-open",
        "session.open",
        json!({"target_id": "chrome_replacing_1", "role": "actor"}),
        1_000,
    );
    assert_eq!(replacement.exit_code, 0, "{}", replacement.value);
    assert_eq!(replacement.value["target_generation"], 2);
    let replacement_session = replacement.value["session_id"].as_str().unwrap();
    assert_eq!(
        invoke(
            &runtime,
            "generation-one-close",
            "session.close",
            json!({"session_id": first_session}),
            1_000,
        )
        .exit_code,
        0
    );
    let new_action = invoke(
        &runtime,
        "generation-two-action",
        "action.click",
        json!({"session_id": replacement_session, "locator": {"kind": "semantic", "name": "New"}}),
        1_000,
    );
    assert_eq!(new_action.value["outcome"], "observed");
}

#[test]
fn locators_modes_deduplication_and_stabilization_are_fail_closed() {
    let harness = Harness::new();
    let macos = harness.open("macos_fake_1", "actor");
    let foreground_required = harness.invoke(
        "mac-press",
        "action.press",
        json!({"session_id": macos, "key": "Enter"}),
    );
    assert_eq!(
        error_code(&foreground_required),
        Some("foreground_required"),
        "{}",
        foreground_required.value
    );
    assert_eq!(foreground_required.value["outcome"], "not_performed");
    assert_eq!(foreground_required.value["delivery"], "not_dispatched");

    let actor = harness.open("chrome_fake_1", "actor");
    let query = harness.invoke(
        "query-1",
        "observe.query",
        json!({"session_id": actor, "semantic": {"kind": "semantic", "role": "button", "name": "Save"}}),
    );
    let reference = query.value["matches"][0]["ref"].as_str().unwrap();
    let request = Invocation::new(
        "action.click",
        json!({"session_id": actor, "locator": {"kind": "ref", "ref": reference}}),
        "dedupe-click".to_owned(),
        1_000,
    );
    let first = harness.runtime.invoke(request.clone());
    let replay = harness.runtime.invoke(request);
    assert_eq!(first.value, replay.value);
    let stale = harness.invoke(
        "stale-ref",
        "action.click",
        json!({"session_id": actor, "locator": {"kind": "ref", "ref": reference}}),
    );
    assert_eq!(error_code(&stale), Some("element_stale"), "{}", stale.value);

    let conflict = harness.runtime.invoke(Invocation::new(
        "action.click",
        json!({"session_id": actor, "locator": {"kind": "semantic", "name": "Other"}}),
        "dedupe-click".to_owned(),
        1_000,
    ));
    assert_eq!(error_code(&conflict), Some("request_id_conflict"));

    let race = invoke(
        &harness.runtime,
        "race",
        "raw.cdp",
        json!({"session_id": actor, "intent": "action", "method": "Fake.race", "params": {}}),
        500,
    );
    assert_eq!(race.value["outcome"], "observed");
    let timeout = invoke(
        &harness.runtime,
        "continuous",
        "raw.cdp",
        json!({"session_id": actor, "intent": "action", "method": "Fake.continuous", "params": {}}),
        80,
    );
    assert_eq!(error_code(&timeout), Some("stabilization_timeout"));
    assert!(timeout.value["observation"]["screenshot_path"].is_null());
}

#[test]
fn simultaneous_identical_request_ids_dispatch_exactly_once() {
    let harness = Harness::new();
    let actor = harness.open("chrome_fake_1", "actor");
    let request = Invocation::new(
        "action.click",
        json!({"session_id": actor, "locator": {"kind": "semantic", "name": "Save"}}),
        "simultaneous-dedupe".to_owned(),
        1_000,
    );
    let barrier = Arc::new(Barrier::new(3));
    let invoke_once = |runtime: Arc<Runtime>, barrier: Arc<Barrier>, request: Invocation| {
        thread::spawn(move || {
            barrier.wait();
            runtime.invoke(request)
        })
    };
    let first = invoke_once(harness.runtime.clone(), barrier.clone(), request.clone());
    let second = invoke_once(harness.runtime.clone(), barrier.clone(), request);
    barrier.wait();
    let first = first.join().unwrap();
    let second = second.join().unwrap();
    assert_eq!(first.value, second.value);
    assert_eq!(first.value["action_sequence"], 1);

    let next = harness.invoke(
        "after-simultaneous-dedupe",
        "action.click",
        json!({"session_id": actor, "locator": {"kind": "semantic", "name": "Next"}}),
    );
    assert_eq!(next.value["action_sequence"], 2);
}

#[test]
fn cancellation_is_priority_and_busy_close_does_not_delete_early() {
    let harness = Harness::new();
    let actor = harness.open("chrome_fake_1", "actor");
    let runtime = harness.runtime.clone();
    let action_session = actor.clone();
    let action = thread::spawn(move || {
        invoke(
            &runtime,
            "blocking-action",
            "raw.cdp",
            json!({
                "session_id": action_session,
                "intent": "action",
                "method": "Fake.block",
                "params": {}
            }),
            1_000,
        )
    });
    thread::sleep(Duration::from_millis(20));
    let busy = harness.invoke("busy-close", "session.close", json!({"session_id": actor}));
    assert_eq!(error_code(&busy), Some("session_busy"));
    let queued_runtime = harness.runtime.clone();
    let queued_session = actor.clone();
    let queued = thread::spawn(move || {
        invoke(
            &queued_runtime,
            "queued-action",
            "action.click",
            json!({"session_id": queued_session, "locator": {"kind": "semantic", "name": "Queued"}}),
            1_000,
        )
    });
    thread::sleep(Duration::from_millis(20));
    let queued_cancel = harness.invoke(
        "cancel-queued",
        "request.cancel",
        json!({"session_id": actor, "request_id": "queued-action"}),
    );
    assert_eq!(queued_cancel.value["disposition"], "cancellation_requested");
    let cancel = harness.invoke(
        "cancel-1",
        "request.cancel",
        json!({"session_id": actor, "request_id": "blocking-action"}),
    );
    assert_eq!(cancel.value["disposition"], "cancellation_requested");
    let terminal = action.join().unwrap();
    assert_eq!(error_code(&terminal), Some("cancelled"));
    let queued_terminal = queued.join().unwrap();
    assert_eq!(error_code(&queued_terminal), Some("cancelled"));
    assert_eq!(queued_terminal.value["outcome"], "not_performed");
    assert_eq!(queued_terminal.value["delivery"], "not_dispatched");
    assert_eq!(queued_terminal.value["error"]["effects"], "none");
    let close = harness.invoke(
        "close-after-cancel",
        "session.close",
        json!({"session_id": actor}),
    );
    assert_eq!(close.exit_code, 0);
}

#[test]
fn target_queue_deadline_expires_without_dispatch() {
    let harness = Harness::new();
    let actor = harness.open("chrome_fake_1", "actor");
    let blocker_runtime = harness.runtime.clone();
    let blocker_session = actor.clone();
    let blocker = thread::spawn(move || {
        invoke(
            &blocker_runtime,
            "deadline-blocker",
            "raw.cdp",
            json!({
                "session_id": blocker_session,
                "intent": "action",
                "method": "Fake.block",
                "params": {}
            }),
            1_000,
        )
    });
    thread::sleep(Duration::from_millis(20));
    let queued_runtime = harness.runtime.clone();
    let queued_session = actor.clone();
    let queued = thread::spawn(move || {
        invoke(
            &queued_runtime,
            "deadline-queued",
            "action.click",
            json!({"session_id": queued_session, "locator": {"kind": "semantic", "name": "Never dispatched"}}),
            50,
        )
    });
    thread::sleep(Duration::from_millis(80));
    harness.invoke(
        "cancel-deadline-blocker",
        "request.cancel",
        json!({"session_id": actor, "request_id": "deadline-blocker"}),
    );
    assert_eq!(error_code(&blocker.join().unwrap()), Some("cancelled"));
    let terminal = queued.join().unwrap();
    assert_eq!(error_code(&terminal), Some("timed_out"));
    assert_eq!(terminal.value["outcome"], "not_performed");
    assert_eq!(terminal.value["delivery"], "not_dispatched");
    assert_eq!(terminal.value["action_sequence"], Value::Null);
    let next = harness.invoke(
        "deadline-next",
        "action.click",
        json!({"session_id": actor, "locator": {"kind": "semantic", "name": "Next"}}),
    );
    assert_eq!(next.value["action_sequence"], 2);
}

#[test]
fn admitted_action_deadline_returns_timed_out_after_possible_dispatch() {
    let harness = Harness::new();
    let actor = harness.open("chrome_fake_1", "actor");
    let terminal = invoke(
        &harness.runtime,
        "admitted-deadline",
        "raw.cdp",
        json!({
            "session_id": actor,
            "intent": "action",
            "method": "Fake.block",
            "params": {}
        }),
        100,
    );

    assert_eq!(
        error_code(&terminal),
        Some("timed_out"),
        "{}",
        terminal.value
    );
    assert_eq!(terminal.value["outcome"], "uncertain");
    assert_eq!(terminal.value["delivery"], "unknown");
}

#[test]
fn adapter_panic_becomes_terminal_internal_error_and_releases_admission() {
    let harness = Harness::new();
    let actor = harness.open("chrome_fake_1", "actor");
    let failed = harness.invoke(
        "panic-action",
        "raw.cdp",
        json!({
            "session_id": actor,
            "intent": "action",
            "method": "Fake.panic",
            "params": {}
        }),
    );
    assert_eq!(error_code(&failed), Some("internal_error"));
    assert_eq!(failed.value["outcome"], "uncertain");
    assert_eq!(failed.value["action_sequence"], 1);

    let recovered = harness.invoke(
        "after-panic",
        "action.click",
        json!({"session_id": actor, "locator": {"kind": "semantic", "name": "Recovered"}}),
    );
    assert_eq!(recovered.value["action_sequence"], 2);
    assert_eq!(
        harness
            .invoke(
                "close-after-panic",
                "session.close",
                json!({"session_id": actor})
            )
            .exit_code,
        0
    );
}

#[test]
fn concurrent_observation_discloses_sequence_race() {
    let root = tempfile::tempdir().unwrap();
    let temporary = root.path().join("tmp");
    let config = root.path().join("config");
    let (observation_started, observation_admitted) = mpsc::sync_channel(0);
    let adapter = Arc::new(SequenceRaceAdapter {
        observation_started: Mutex::new(Some(observation_started)),
        observation_release: Barrier::new(2),
    });
    let harness = Harness {
        _root: root,
        config: config.clone(),
        runtime: Arc::new(
            Runtime::new(
                RuntimeConfig {
                    temporary_root: temporary,
                    config_root: config,
                },
                vec![adapter.clone()],
            )
            .unwrap(),
        ),
    };
    let actor = harness.open("chrome_fake_1", "actor");
    let observer = harness.open("chrome_fake_1", "observer");
    let runtime = harness.runtime.clone();
    let observer_session = observer.clone();
    let observation = thread::spawn(move || {
        invoke(
            &runtime,
            "concurrent-shot",
            "observe.screenshot",
            json!({"session_id": observer_session}),
            1_000,
        )
    });
    observation_admitted
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let click = harness.invoke(
        "concurrent-click",
        "action.click",
        json!({"session_id": actor, "locator": {"kind": "semantic", "name": "Save"}}),
    );
    adapter.observation_release.wait();
    assert_eq!(click.value["outcome"], "observed");
    let observed = observation.join().unwrap();
    assert_eq!(observed.value["observation_status"], "concurrent");
    assert_ne!(
        observed.value["action_sequence_before"],
        observed.value["action_sequence_after"]
    );
}

#[test]
fn concurrent_artifact_publication_keeps_every_manifest_entry() {
    let harness = Harness::new();
    let observer = harness.open("chrome_fake_1", "observer");
    let barrier = Arc::new(Barrier::new(9));
    let mut workers = Vec::new();
    for index in 0..8 {
        let runtime = harness.runtime.clone();
        let barrier = barrier.clone();
        let session = observer.clone();
        workers.push(thread::spawn(move || {
            barrier.wait();
            invoke(
                &runtime,
                &format!("parallel-shot-{index}"),
                "observe.screenshot",
                json!({"session_id": session}),
                1_000,
            )
        }));
    }
    barrier.wait();
    let replies = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();
    assert!(replies.iter().all(|reply| reply.exit_code == 0));
    let manifest_path = PathBuf::from(replies[0].value["manifest_path"].as_str().unwrap());
    let manifest: Value = serde_json::from_slice(&fs::read(manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["artifacts"].as_array().unwrap().len(), 8);
    for artifact in manifest["artifacts"].as_array().unwrap() {
        assert!(Path::new(artifact["absolute_path"].as_str().unwrap()).is_file());
    }
}

#[test]
fn raw_usage_is_opt_in_private_disableable_and_corruption_safe() {
    let harness = Harness::new();
    let actor = harness.open("chrome_fake_1", "actor");
    let secret = "https://secret.example/path?token=forbidden";
    let disabled = harness.invoke(
        "raw-disabled",
        "raw.cdp",
        json!({"session_id": actor, "intent": "query", "method": "Runtime.evaluate", "params": {"expression": secret}}),
    );
    assert_eq!(disabled.exit_code, 0);
    assert!(!harness.config.join("usage.json").exists());

    assert_eq!(
        harness
            .invoke(
                "usage-enable",
                "system.commands.usage",
                json!({"action": "enable"})
            )
            .exit_code,
        0
    );
    let enabled = harness.invoke(
        "raw-enabled",
        "raw.cdp",
        json!({"session_id": actor, "intent": "query", "method": "Runtime.evaluate", "params": {"expression": secret}}),
    );
    assert_eq!(enabled.exit_code, 0);
    let usage_path = harness.config.join("usage.json");
    let allowed = fs::read_to_string(&usage_path).unwrap();
    assert!(allowed.contains("Runtime.evaluate"));
    assert!(!allowed.contains(secret));
    assert!(!allowed.contains("expression"));

    assert_eq!(
        harness
            .invoke(
                "usage-disable",
                "system.commands.usage",
                json!({"action": "disable"})
            )
            .exit_code,
        0
    );
    let before = fs::read(&usage_path).unwrap();
    harness.invoke(
        "raw-disabled-again",
        "raw.cdp",
        json!({"session_id": actor, "intent": "query", "method": "Runtime.evaluate", "params": {}}),
    );
    assert_eq!(fs::read(&usage_path).unwrap(), before);

    harness.invoke(
        "usage-enable-again",
        "system.commands.usage",
        json!({"action": "enable"}),
    );
    fs::write(&usage_path, b"not-json").unwrap();
    let corrupt_before = fs::read(&usage_path).unwrap();
    let warning = harness.invoke(
        "raw-corrupt",
        "raw.cdp",
        json!({"session_id": actor, "intent": "query", "method": "Runtime.evaluate", "params": {}}),
    );
    assert_eq!(warning.value["warning"], "usage_not_recorded");
    assert_eq!(fs::read(&usage_path).unwrap(), corrupt_before);
    let export_path = harness._root.path().join("corrupt-export.json");
    let export = harness.invoke(
        "usage-export",
        "system.commands.usage",
        json!({"action": "export", "destination": export_path}),
    );
    assert_eq!(export.exit_code, 0);
    assert_eq!(fs::read(export_path).unwrap(), corrupt_before);
    assert_eq!(
        harness
            .invoke(
                "usage-reset",
                "system.commands.usage",
                json!({"action": "reset"})
            )
            .exit_code,
        0
    );
}

#[test]
fn restart_cleans_only_verified_orphans_and_frees_actor_lease() {
    let root = tempfile::tempdir().unwrap();
    let temporary = root.path().join("tmp");
    let config = root.path().join("config");
    let first = runtime(&temporary, &config);
    let opened = invoke(
        &first,
        "open-before-crash",
        "session.open",
        json!({"target_id": "chrome_fake_1", "role": "actor"}),
        1_000,
    );
    let session_id = opened.value["session_id"].as_str().unwrap();
    let shot = invoke(
        &first,
        "shot-before-crash",
        "observe.screenshot",
        json!({"session_id": session_id}),
        1_000,
    );
    let orphan = Path::new(shot.value["screenshot_path"].as_str().unwrap())
        .parent()
        .unwrap()
        .to_path_buf();
    let observer = invoke(
        &first,
        "open-tainted-orphan",
        "session.open",
        json!({"target_id": "chrome_fake_1", "role": "observer"}),
        1_000,
    );
    let observer_id = observer.value["session_id"].as_str().unwrap();
    let observer_shot = invoke(
        &first,
        "shot-tainted-orphan",
        "observe.screenshot",
        json!({"session_id": observer_id}),
        1_000,
    );
    let tainted_orphan = Path::new(observer_shot.value["screenshot_path"].as_str().unwrap())
        .parent()
        .unwrap()
        .to_path_buf();
    let sessions_root = temporary.join("manuvra/sessions-v1");
    let malformed = sessions_root.join("foreign-directory");
    fs::create_dir(&malformed).unwrap();
    let link = sessions_root.join("foreign-symlink");
    symlink(root.path(), &link).unwrap();
    drop(first);
    fs::write(tainted_orphan.join("foreign.txt"), b"preserve me").unwrap();

    let second = runtime(&temporary, &config);
    assert!(!orphan.exists());
    assert!(tainted_orphan.exists());
    assert_eq!(
        fs::read(tainted_orphan.join("foreign.txt")).unwrap(),
        b"preserve me"
    );
    assert!(malformed.exists());
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    let doctor = invoke(
        &second,
        "doctor-after-cleanup",
        "system.doctor",
        json!({}),
        1_000,
    );
    assert!(
        doctor.value["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning == "unverified_orphans_preserved")
    );
    let reopened = invoke(
        &second,
        "open-after-crash",
        "session.open",
        json!({"target_id": "chrome_fake_1", "role": "actor"}),
        1_000,
    );
    assert_eq!(reopened.exit_code, 0, "{}", reopened.value);
}

#[test]
fn manifest_paths_and_hashes_are_not_trusted_for_export_or_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let temporary = root.path().join("tmp");
    let config = root.path().join("config");
    let first = runtime(&temporary, &config);
    let actor = invoke(
        &first,
        "open-tamper",
        "session.open",
        json!({"target_id": "chrome_fake_1", "role": "actor"}),
        1_000,
    );
    let session_id = actor.value["session_id"].as_str().unwrap();
    let shot = invoke(
        &first,
        "shot-tamper",
        "observe.screenshot",
        json!({"session_id": session_id}),
        1_000,
    );
    let screenshot = PathBuf::from(shot.value["screenshot_path"].as_str().unwrap());
    let directory = screenshot.parent().unwrap().to_path_buf();
    let manifest_path = directory.join("manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    let outside = root.path().join("private-outside.txt");
    fs::write(&outside, b"outside secret").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
    manifest["artifacts"][0]["absolute_path"] = json!(outside);
    manifest["artifacts"][0]["bytes"] = json!(14);
    manifest["artifacts"][0]["sha256"] = json!(manuvra_protocol::sha256_hex(b"outside secret"));
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

    let export = invoke(
        &first,
        "export-tampered",
        "artifact.export",
        json!({"session_id": session_id, "all": true, "destination": root.path().join("export")}),
        1_000,
    );
    assert_eq!(error_code(&export), Some("export_failed"));
    drop(first);
    let _second = runtime(&temporary, &config);
    assert!(directory.exists());
    assert_eq!(fs::read(&outside).unwrap(), b"outside secret");
}

#[test]
fn every_observable_reply_in_contract_suite_is_bounded_json() {
    let harness = Harness::new();
    let actor = harness.open("chrome_fake_1", "actor");
    let replies = [
        harness.invoke("bounded-targets", "target.list", json!({})),
        harness.invoke("bounded-doctor", "system.doctor", json!({})),
        harness.invoke(
            "bounded-show",
            "system.commands.usage",
            json!({"action": "show"}),
        ),
        harness.invoke(
            "bounded-error",
            "action.click",
            json!({"session_id": actor, "locator": {"kind": "semantic", "name": "Missing"}}),
        ),
    ];
    for reply in replies {
        let bytes = encode_operational_line(&reply.value).unwrap();
        assert!(bytes.len() <= 4096);
        serde_json::from_slice::<Value>(&bytes).unwrap();
    }
}

#[test]
fn system_setup_routes_to_the_permission_owner_and_returns_rechecked_facts() {
    let harness = Harness::new();

    let reply = harness.invoke("focused-setup", "system.setup", json!({}));

    assert_eq!(reply.exit_code, 0);
    validate_command_result("system.setup", &reply.value).unwrap();
    for permission in ["accessibility", "screen_recording", "post_event"] {
        let fact = &reply.value["permissions"][permission];
        assert_eq!(fact["before_granted"], true);
        assert_eq!(fact["prompt_requested"], false);
        assert_eq!(fact["settings_opened"], false);
        assert_eq!(fact["granted"], true);
        assert_eq!(fact["freshly_granted"], false);
        assert_eq!(fact["residual"], false);
    }
}

#[test]
fn system_setup_caches_installation_and_adapter_side_effects_by_request_id() {
    let root = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Arc::new(
        Runtime::new(
            RuntimeConfig {
                temporary_root: root.path().join("tmp"),
                config_root: root.path().join("config"),
            },
            vec![Arc::new(SetupTrackingAdapter {
                calls: calls.clone(),
                timeout: false,
            })],
        )
        .unwrap()
        .with_setup_installation(json!({
            "installed": true,
            "bundle": "/opt/manuvra/Manuvra.app"
        })),
    );

    let first = invoke(&runtime, "same-setup", "system.setup", json!({}), 1_000);
    let replay = invoke(&runtime, "same-setup", "system.setup", json!({}), 750);

    assert_eq!(first.value, replay.value);
    assert_eq!(
        first.value["installation"]["bundle"],
        "/opt/manuvra/Manuvra.app"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn timed_out_system_setup_is_terminal_and_retry_does_not_replay_side_effects() {
    let root = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = Arc::new(
        Runtime::new(
            RuntimeConfig {
                temporary_root: root.path().join("tmp"),
                config_root: root.path().join("config"),
            },
            vec![Arc::new(SetupTrackingAdapter {
                calls: calls.clone(),
                timeout: true,
            })],
        )
        .unwrap(),
    );

    let first = invoke(&runtime, "same-timeout", "system.setup", json!({}), 1_000);
    let replay = invoke(&runtime, "same-timeout", "system.setup", json!({}), 1_000);

    assert_eq!(error_code(&first), Some("timed_out"));
    assert_eq!(first.value, replay.value);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn all_registry_commands_reach_the_shared_interface() {
    let harness = Harness::new();
    let chrome = harness.open("chrome_fake_1", "actor");
    let macos = harness.open("macos_fake_1", "actor");
    let destination = harness._root.path().join("matrix-export");
    let mut replies = vec![
        harness.invoke("matrix-list", "system.commands.list", json!({})),
        harness.invoke("matrix-get", "system.commands.get", json!({"command": "action.click"})),
        harness.invoke("matrix-schema", "system.commands.schema", json!({"command": "action.click", "side": "input"})),
        harness.invoke("matrix-errors", "system.commands.errors", json!({"code": "foreground_required"})),
        harness.invoke("matrix-usage", "system.commands.usage", json!({"action": "show"})),
        harness.invoke("matrix-targets", "target.list", json!({})),
        harness.invoke("matrix-lease-release", "lease.manage", json!({"session_id": chrome, "action": "release"})),
        harness.invoke("matrix-lease-acquire", "lease.manage", json!({"session_id": chrome, "action": "acquire"})),
        harness.invoke("matrix-click", "action.click", json!({"session_id": chrome, "locator": {"kind": "semantic", "name": "Save"}})),
        harness.invoke("matrix-type", "action.type", json!({"session_id": chrome, "locator": {"kind": "semantic", "role": "textbox"}, "text": "hello"})),
        harness.invoke("matrix-press", "action.press", json!({"session_id": chrome, "key": "Enter"})),
        harness.invoke("matrix-scroll", "action.scroll", json!({"session_id": chrome, "delta_x": 0, "delta_y": 20})),
        harness.invoke("matrix-navigate", "action.navigate", json!({"session_id": chrome, "url": "https://example.test"})),
        harness.invoke("matrix-query", "observe.query", json!({"session_id": chrome, "semantic": {"kind": "semantic", "name": "Save"}})),
        harness.invoke("matrix-shot", "observe.screenshot", json!({"session_id": chrome})),
        harness.invoke("matrix-tree", "observe.tree", json!({"session_id": chrome})),
        harness.invoke("matrix-evidence", "observe.evidence", json!({"session_id": chrome, "kind": "logs"})),
        harness.invoke("matrix-cdp-query", "raw.cdp", json!({"session_id": chrome, "intent": "query", "method": "Runtime.evaluate", "params": {}})),
        harness.invoke("matrix-cdp-action", "raw.cdp", json!({"session_id": chrome, "intent": "action", "method": "Runtime.evaluate", "params": {}})),
        harness.invoke("matrix-cancel", "request.cancel", json!({"session_id": chrome, "request_id": "already-terminal"})),
        harness.invoke("matrix-doctor", "system.doctor", json!({})),
        harness.invoke("matrix-daemon-status", "daemon.status", json!({})),
        harness.invoke("matrix-daemon-stop", "daemon.stop", json!({})),
        harness.invoke("matrix-chrome-launch", "system.chrome.launch", json!({})),
        harness.invoke("matrix-setup", "system.setup", json!({})),
        harness.invoke("matrix-migrate", "system.migrate", json!({"from": "computer-use"})),
        harness.invoke("matrix-purge", "system.purge", json!({"all": true, "yes": true})),
    ];

    let ax_query = harness.invoke(
        "matrix-ax-query-1",
        "observe.query",
        json!({"session_id": macos, "semantic": {"kind": "semantic", "name": "Save"}}),
    );
    let ax_ref = ax_query.value["matches"][0]["ref"]
        .as_str()
        .unwrap()
        .to_owned();
    replies.push(harness.invoke(
        "matrix-ax-get",
        "raw.ax.get",
        json!({"session_id": macos, "ref": ax_ref, "attribute": "AXValue"}),
    ));
    let ax_query = harness.invoke(
        "matrix-ax-query-2",
        "observe.query",
        json!({"session_id": macos, "semantic": {"kind": "semantic", "name": "Save"}}),
    );
    let ax_ref = ax_query.value["matches"][0]["ref"]
        .as_str()
        .unwrap()
        .to_owned();
    replies.push(harness.invoke(
        "matrix-ax-set",
        "raw.ax.set",
        json!({
            "session_id": macos, "ref": ax_ref, "attribute": "AXValue",
            "value": {"type": "string", "value": "hello"}
        }),
    ));
    let ax_query = harness.invoke(
        "matrix-ax-query-3",
        "observe.query",
        json!({"session_id": macos, "semantic": {"kind": "semantic", "name": "Save"}}),
    );
    let ax_ref = ax_query.value["matches"][0]["ref"]
        .as_str()
        .unwrap()
        .to_owned();
    replies.push(harness.invoke(
        "matrix-ax-perform",
        "raw.ax.perform",
        json!({
            "session_id": macos, "ref": ax_ref, "action": "AXPress"
        }),
    ));
    replies.push(harness.invoke(
        "matrix-export",
        "artifact.export",
        json!({"session_id": chrome, "all": true, "destination": destination}),
    ));
    replies.push(harness.invoke(
        "matrix-close",
        "session.close",
        json!({"session_id": chrome}),
    ));

    assert_eq!(
        manuvra_protocol::registry()["commands"]
            .as_array()
            .unwrap()
            .len(),
        31
    );
    for reply in replies {
        assert!(
            encode_operational_line(&reply.value).unwrap().len() <= 4096,
            "{}",
            reply.value
        );
        assert_ne!(
            error_code(&reply),
            Some("unknown_command"),
            "{}",
            reply.value
        );
    }
}

#[test]
fn every_registry_actor_command_rejects_an_observer_session() {
    let harness = Harness::new();
    let chrome_observer = harness.open("chrome_fake_1", "observer");
    let macos_observer = harness.open("macos_fake_1", "observer");
    let actor_commands = manuvra_protocol::registry()["commands"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|command| command["authority"] == "actor")
        .collect::<Vec<_>>();

    for command in &actor_commands {
        let id = command["id"].as_str().unwrap();
        let mut input = command["examples"][0]["input"].clone();
        input["session_id"] = json!(if id.starts_with("raw.ax") {
            &macos_observer
        } else {
            &chrome_observer
        });
        let reply = harness.invoke(&format!("authority-{}", id.replace('.', "-")), id, input);
        assert_eq!(
            error_code(&reply),
            Some("actor_lease_required"),
            "registry actor authority was not enforced for {id}: {}",
            reply.value
        );
    }
    assert_eq!(actor_commands.len(), 8);
}

#[test]
fn lease_observe_mutate_and_artifact_results_stay_at_public_runtime_seams() {
    let harness = Harness::new();
    let chrome_kind = harness.invoke(
        "kind-chrome",
        "target.list",
        json!({"kind": "chrome", "limit": 10}),
    );
    assert_eq!(chrome_kind.value["targets"].as_array().unwrap().len(), 1);
    assert_eq!(chrome_kind.value["targets"][0]["kind"], "chrome");
    assert_eq!(
        error_code(&harness.invoke("kind-invalid", "target.list", json!({"kind": "safari"}))),
        Some("invalid_request")
    );

    let actor = harness.open("chrome_fake_1", "actor");
    let observer = harness.open("chrome_fake_1", "observer");
    let held = harness.invoke("targets-held", "target.list", json!({"limit": 10}));
    let chrome = held.value["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|target| target["kind"] == "chrome")
        .unwrap();
    assert_eq!(chrome["actor_lease"], "held");

    let released = harness.invoke(
        "lease-release",
        "lease.manage",
        json!({"session_id": actor, "action": "release"}),
    );
    assert_eq!(released.value["state"], "released");
    let after_release = harness.invoke(
        "click-without-lease",
        "action.click",
        json!({"session_id": actor, "locator": {"kind": "semantic", "name": "Save"}}),
    );
    assert_eq!(error_code(&after_release), Some("actor_lease_expired"));
    assert_eq!(after_release.value["outcome"], "not_performed");
    assert_eq!(after_release.value["delivery"], "not_dispatched");

    let acquired = harness.invoke(
        "lease-acquire",
        "lease.manage",
        json!({"session_id": actor, "action": "acquire"}),
    );
    assert_eq!(acquired.value["state"], "held");
    let renewed = harness.invoke(
        "lease-renew",
        "lease.manage",
        json!({"session_id": actor, "action": "renew", "ttl_ms": 30_000}),
    );
    assert_eq!(renewed.value["state"], "held");
    assert_eq!(renewed.value["ttl_ms"], 30_000);
    assert_eq!(
        error_code(&harness.invoke(
            "lease-acquire-held",
            "lease.manage",
            json!({"session_id": actor, "action": "acquire"})
        )),
        Some("actor_lease_held")
    );
    assert_eq!(
        error_code(&harness.invoke(
            "observer-lease",
            "lease.manage",
            json!({"session_id": observer, "action": "acquire"})
        )),
        Some("actor_lease_required")
    );

    let query = harness.invoke(
        "observe-query",
        "observe.query",
        json!({"session_id": actor, "semantic": {"kind": "semantic", "name": "Save"}}),
    );
    assert_eq!(query.value["matches"].as_array().unwrap().len(), 1);
    assert_eq!(query.value["observation_status"], "stable");
    assert_eq!(
        error_code(&harness.invoke(
            "observe-missing",
            "observe.query",
            json!({"session_id": actor, "semantic": {"kind": "semantic", "name": "Missing"}})
        )),
        Some("element_not_found")
    );

    let shot = harness.invoke(
        "observe-shot",
        "observe.screenshot",
        json!({"session_id": actor}),
    );
    let frame_token = shot.value["frame_token"].as_str().unwrap().to_owned();
    let logs = harness.invoke(
        "observe-logs",
        "observe.evidence",
        json!({"session_id": actor, "kind": "logs"}),
    );
    assert_eq!(logs.value["kind"], "logs");
    let manifest = harness.invoke(
        "observe-manifest",
        "observe.evidence",
        json!({"session_id": actor, "kind": "manifest"}),
    );
    assert_eq!(manifest.value["kind"], "manifest");
    assert!(Path::new(manifest.value["path"].as_str().unwrap()).is_file());
    assert_eq!(
        error_code(&harness.invoke(
            "observe-kind-invalid",
            "observe.evidence",
            json!({"session_id": actor, "kind": "heap"})
        )),
        Some("invalid_request")
    );

    let clicked = harness.invoke(
        "mutate-click",
        "action.click",
        json!({
            "session_id": actor,
            "locator": {"kind": "point", "x": 1, "y": 1, "frame_token": frame_token}
        }),
    );
    assert_eq!(clicked.value["outcome"], "observed");
    assert_eq!(clicked.value["delivery"], "backend_confirmed");
    assert_eq!(clicked.value["effect_verification"], "not_asserted");
    assert_eq!(
        error_code(&harness.invoke(
            "stale-frame",
            "action.click",
            json!({
                "session_id": actor,
                "locator": {"kind": "point", "x": 1, "y": 1, "frame_token": frame_token}
            })
        )),
        Some("frame_stale")
    );

    let rejected = harness.invoke(
        "mutate-reject",
        "raw.cdp",
        json!({"session_id": actor, "intent": "action", "method": "Fake.reject", "params": {}}),
    );
    assert_eq!(rejected.value["outcome"], "not_performed");
    assert_eq!(rejected.value["delivery"], "backend_rejected");
    let uncertain = harness.invoke(
        "mutate-ambiguous",
        "raw.cdp",
        json!({"session_id": actor, "intent": "action", "method": "Fake.ambiguous", "params": {}}),
    );
    assert_eq!(uncertain.value["outcome"], "uncertain");
    assert_eq!(uncertain.value["delivery"], "unknown");
    let interrupted = harness.invoke(
        "mutate-interrupt",
        "raw.cdp",
        json!({"session_id": actor, "intent": "action", "method": "Fake.interrupt", "params": {}}),
    );
    assert_eq!(error_code(&interrupted), Some("interrupted"));
    assert_eq!(interrupted.value["outcome"], "uncertain");

    let artifact_id = serde_json::from_slice::<Value>(
        &fs::read(Path::new(shot.value["manifest_path"].as_str().unwrap())).unwrap(),
    )
    .unwrap()["artifacts"][0]["artifact_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let selected_dest = harness._root.path().join("selected-export");
    let selected = harness.invoke(
        "export-selected",
        "artifact.export",
        json!({
            "session_id": actor,
            "artifact_ids": [artifact_id],
            "destination": selected_dest
        }),
    );
    assert_eq!(selected.value["verified"], true);
    assert_eq!(selected.value["files"].as_array().unwrap().len(), 1);
    assert_eq!(
        selected.value["files"][0]["artifact_id"].as_str().unwrap(),
        artifact_id
    );
    assert!(!selected_dest.join("manifest.json").exists());
    assert_eq!(
        error_code(&harness.invoke(
            "export-both",
            "artifact.export",
            json!({
                "session_id": actor,
                "all": true,
                "artifact_ids": [artifact_id],
                "destination": harness._root.path().join("both")
            })
        )),
        Some("invalid_request")
    );
    assert_eq!(
        error_code(&harness.invoke(
            "export-relative",
            "artifact.export",
            json!({"session_id": actor, "all": true, "destination": "relative-export"})
        )),
        Some("invalid_request")
    );
}

#[test]
fn cancel_running_close_waits_until_session_work_is_idle() {
    let harness = Harness::new();
    let actor = harness.open("chrome_fake_1", "actor");
    let runtime = harness.runtime.clone();
    let action_session = actor.clone();
    let action = thread::spawn(move || {
        invoke(
            &runtime,
            "close-cancel-block",
            "raw.cdp",
            json!({
                "session_id": action_session,
                "intent": "action",
                "method": "Fake.block",
                "params": {}
            }),
            1_000,
        )
    });
    thread::sleep(Duration::from_millis(20));
    let closed = harness.invoke(
        "close-cancel-running",
        "session.close",
        json!({"session_id": actor, "cancel_running": true}),
    );
    assert_eq!(closed.exit_code, 0, "{}", closed.value);
    assert_eq!(closed.value["closed"], true);
    assert_eq!(error_code(&action.join().unwrap()), Some("cancelled"));
}

#[test]
fn request_identity_and_discovery_keep_envelope_and_schema_results() {
    let harness = Harness::new();
    let short_deadline = harness.runtime.invoke(Invocation::new(
        "target.list",
        json!({}),
        "short-deadline".to_owned(),
        1,
    ));
    assert_eq!(error_code(&short_deadline), Some("invalid_request"));
    let empty_id = harness.runtime.invoke(Invocation::new(
        "target.list",
        json!({}),
        String::new(),
        1_000,
    ));
    assert_eq!(error_code(&empty_id), Some("invalid_request"));
    assert_eq!(
        error_code(&harness.invoke(
            "schema-unknown",
            "system.commands.schema",
            json!({"command": "not.a.command", "side": "input"})
        )),
        Some("unknown_command")
    );
    assert_eq!(
        error_code(&harness.invoke(
            "schema-side",
            "system.commands.schema",
            json!({"command": "action.click", "side": "docs"})
        )),
        Some("invalid_request")
    );
    let input_schema = harness.invoke(
        "schema-input",
        "system.commands.schema",
        json!({"command": "action.click", "side": "input"}),
    );
    assert_eq!(input_schema.exit_code, 0, "{}", input_schema.value);
    let result_schema = harness.invoke(
        "schema-result",
        "system.commands.schema",
        json!({"command": "action.click", "side": "result"}),
    );
    assert_eq!(result_schema.exit_code, 0, "{}", result_schema.value);
    assert_eq!(
        error_code(&harness.invoke(
            "usage-unknown",
            "system.commands.usage",
            json!({"action": "wipe"})
        )),
        Some("invalid_request")
    );
    assert_eq!(
        error_code(&harness.invoke(
            "open-role",
            "session.open",
            json!({"target_id": "chrome_fake_1", "role": "admin"})
        )),
        Some("invalid_request")
    );
    let observer = harness.open("chrome_fake_1", "observer");
    assert!(observer.starts_with("s_"));
    assert_eq!(
        error_code(&harness.invoke("list-limit", "target.list", json!({"limit": 0}))),
        Some("invalid_request")
    );
    assert_eq!(
        error_code(&harness.invoke("list-cursor", "target.list", json!({"cursor": "nope"}))),
        Some("invalid_request")
    );
    let page = harness.invoke("list-page", "target.list", json!({"limit": 1}));
    assert_eq!(page.value["targets"].as_array().unwrap().len(), 1);
    assert_eq!(page.value["next_cursor"], "1");
    assert_eq!(
        error_code(&harness.invoke("schema-limit", "system.commands.list", json!({"limit": 11}))),
        Some("invalid_request")
    );
    assert_eq!(
        error_code(&harness.invoke(
            "click-mode",
            "action.click",
            json!({
                "session_id": observer,
                "locator": {"kind": "semantic", "name": "Save"},
                "mode": "sideways"
            })
        )),
        Some("invalid_request")
    );
    let foreground = harness.invoke(
        "click-foreground",
        "action.click",
        json!({
            "session_id": observer,
            "locator": {"kind": "semantic", "name": "Save"},
            "mode": "foreground"
        }),
    );
    assert_eq!(error_code(&foreground), Some("actor_lease_required"));
    assert_eq!(
        error_code(&harness.invoke(
            "export-missing",
            "artifact.export",
            json!({
                "session_id": "s_missing",
                "all": true,
                "destination": harness._root.path().join("missing-export")
            })
        )),
        Some("session_not_found")
    );
    assert_eq!(
        error_code(&harness.invoke(
            "click-missing",
            "action.click",
            json!({"session_id": "s_missing", "locator": {"kind": "semantic", "name": "Save"}})
        )),
        Some("session_not_found")
    );
}
