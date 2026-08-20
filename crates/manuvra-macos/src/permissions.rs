use objc2_core_foundation::{CFBoolean, CFDictionary, CFString};
use objc2_core_graphics::{
    CGPreflightPostEventAccess, CGPreflightScreenCaptureAccess, CGRequestPostEventAccess,
    CGRequestScreenCaptureAccess,
};
use serde::Serialize;
use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SetupPermissionsError {
    Deadline,
    Settings(&'static str),
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct PermissionSnapshot {
    pub accessibility: bool,
    pub screen_recording: bool,
    pub post_event: bool,
}

#[derive(Clone, Copy)]
struct Permission {
    name: &'static str,
    settings_url: &'static str,
    granted: fn(PermissionSnapshot) -> bool,
    request: fn(),
}

impl Permission {
    fn name(self) -> &'static str {
        self.name
    }

    fn granted_in(self, snapshot: PermissionSnapshot) -> bool {
        (self.granted)(snapshot)
    }

    fn settings_url(self) -> &'static str {
        self.settings_url
    }
}

const ACCESSIBILITY_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";
const SCREEN_RECORDING_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";

const PERMISSIONS: [Permission; 3] = [
    Permission {
        name: "accessibility",
        settings_url: ACCESSIBILITY_SETTINGS_URL,
        granted: |snapshot| snapshot.accessibility,
        request: request_accessibility,
    },
    Permission {
        name: "screen_recording",
        settings_url: SCREEN_RECORDING_SETTINGS_URL,
        granted: |snapshot| snapshot.screen_recording,
        request: request_screen_recording,
    },
    Permission {
        name: "post_event",
        settings_url: ACCESSIBILITY_SETTINGS_URL,
        granted: |snapshot| snapshot.post_event,
        request: request_post_event,
    },
];

trait PermissionApi {
    fn snapshot(&self) -> PermissionSnapshot;
    fn request(&self, permission: Permission);
    fn open_settings(
        &self,
        url: &'static str,
        deadline: Instant,
    ) -> Result<bool, SetupPermissionsError>;
}

struct SystemPermissionApi;

impl PermissionApi for SystemPermissionApi {
    fn snapshot(&self) -> PermissionSnapshot {
        PermissionSnapshot::current()
    }

    fn request(&self, permission: Permission) {
        // Setup is the sole caller of these public request APIs. They execute in the daemon
        // process, so macOS evaluates the same identity reported by doctor. The return values are
        // deliberately not treated as grants; a fresh passive snapshot is authoritative.
        (permission.request)();
    }

    fn open_settings(
        &self,
        url: &'static str,
        deadline: Instant,
    ) -> Result<bool, SetupPermissionsError> {
        open_settings_until(url, deadline)
    }
}

pub(crate) fn setup_permissions(
    deadline: Instant,
) -> Result<serde_json::Value, SetupPermissionsError> {
    setup_permissions_with(&SystemPermissionApi, deadline)
}

fn setup_permissions_with(
    api: &impl PermissionApi,
    deadline: Instant,
) -> Result<serde_json::Value, SetupPermissionsError> {
    if Instant::now() >= deadline {
        return Err(SetupPermissionsError::Deadline);
    }
    let before = api.snapshot();
    for permission in PERMISSIONS {
        if !permission.granted_in(before) {
            if Instant::now() >= deadline {
                return Err(SetupPermissionsError::Deadline);
            }
            api.request(permission);
            if Instant::now() >= deadline {
                return Err(SetupPermissionsError::Deadline);
            }
        }
    }
    let after = api.snapshot();
    if Instant::now() >= deadline {
        return Err(SetupPermissionsError::Deadline);
    }
    let mut permissions = PERMISSIONS
        .into_iter()
        .map(|permission| {
            let before_granted = permission.granted_in(before);
            let granted = permission.granted_in(after);
            (
                permission.name().to_owned(),
                serde_json::json!({
                    "before_granted": before_granted,
                    "prompt_requested": !before_granted,
                    "settings_opened": false,
                    "granted": granted,
                    "freshly_granted": !before_granted && granted,
                    "residual": !granted,
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let mut opened_by_url = HashMap::new();
    for permission in PERMISSIONS {
        if permission.granted_in(after) {
            continue;
        }
        if Instant::now() >= deadline {
            return Err(SetupPermissionsError::Deadline);
        }
        let opened = match opened_by_url.get(permission.settings_url()) {
            Some(opened) => *opened,
            None => {
                let opened = api
                    .open_settings(permission.settings_url(), deadline)
                    .map_err(|error| match error {
                        SetupPermissionsError::Deadline => SetupPermissionsError::Deadline,
                        SetupPermissionsError::Settings(_) => {
                            SetupPermissionsError::Settings(permission.name())
                        }
                    })?;
                opened_by_url.insert(permission.settings_url(), opened);
                opened
            }
        };
        permissions[permission.name()]["settings_opened"] = serde_json::Value::Bool(opened);
        if !opened {
            return Err(SetupPermissionsError::Settings(permission.name()));
        }
    }
    Ok(serde_json::json!({"permissions": permissions}))
}

fn settings_open_suppressed() -> bool {
    cfg!(debug_assertions) && std::env::var_os("MANUVRA_TEST_NO_OPEN").is_some()
}

fn open_settings_until(
    url: &'static str,
    deadline: Instant,
) -> Result<bool, SetupPermissionsError> {
    if Instant::now() >= deadline {
        return Err(SetupPermissionsError::Deadline);
    }
    if settings_open_suppressed() {
        return Ok(true);
    }
    spawn_settings_child(url, deadline)
}

fn spawn_settings_child(
    url: &'static str,
    deadline: Instant,
) -> Result<bool, SetupPermissionsError> {
    let mut child = match Command::new("/usr/bin/open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return Err(SetupPermissionsError::Settings("System Settings")),
    };
    wait_for_settings_child(&mut child, deadline)
}

fn wait_for_settings_child(
    child: &mut std::process::Child,
    deadline: Instant,
) -> Result<bool, SetupPermissionsError> {
    loop {
        if let Some(result) = poll_settings_child(child, deadline) {
            return result;
        }
        thread::sleep(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(5)),
        );
    }
}

fn poll_settings_child(
    child: &mut std::process::Child,
    deadline: Instant,
) -> Option<Result<bool, SetupPermissionsError>> {
    if let Some(result) = expired_settings_child(child, deadline) {
        return Some(result);
    }
    settings_child_result(child, deadline)
}

fn expired_settings_child(
    child: &mut std::process::Child,
    deadline: Instant,
) -> Option<Result<bool, SetupPermissionsError>> {
    if Instant::now() < deadline {
        return None;
    }
    let _ = child.kill();
    let _ = child.try_wait();
    Some(Err(SetupPermissionsError::Deadline))
}

fn settings_child_result(
    child: &mut std::process::Child,
    deadline: Instant,
) -> Option<Result<bool, SetupPermissionsError>> {
    match child.try_wait() {
        Ok(status) => completed_child_status(status, deadline),
        Err(_) => Some(Err(SetupPermissionsError::Settings("System Settings"))),
    }
}

fn completed_child_status(
    status: Option<std::process::ExitStatus>,
    deadline: Instant,
) -> Option<Result<bool, SetupPermissionsError>> {
    status.map(|status| completed_settings_result(status.success(), deadline))
}

fn completed_settings_result(
    success: bool,
    deadline: Instant,
) -> Result<bool, SetupPermissionsError> {
    if Instant::now() >= deadline {
        return Err(SetupPermissionsError::Deadline);
    }
    Ok(success)
}

fn request_accessibility() {
    let prompt = CFBoolean::new(true);
    // SAFETY: the framework exports a non-null CFString constant for this key on every supported
    // macOS version. CFDictionary retains both CF values for the duration of the call.
    let key = unsafe { kAXTrustedCheckOptionPrompt }
        .expect("kAXTrustedCheckOptionPrompt is available on supported macOS");
    let options = CFDictionary::from_slices(&[key], &[prompt]);
    unsafe {
        let _ = AXIsProcessTrustedWithOptions(Some(&options));
    }
}

fn request_screen_recording() {
    let _ = CGRequestScreenCaptureAccess();
}

fn request_post_event() {
    let _ = CGRequestPostEventAccess();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MissingPermission {
    Accessibility,
    ScreenRecording,
    PostEvent,
}

impl PermissionSnapshot {
    pub fn current() -> Self {
        // SAFETY: these public preflight functions take no pointers, do not prompt, and return the
        // current TCC disposition for the calling daemon identity.
        unsafe {
            Self {
                accessibility: crate::seam::permission("accessibility", AXIsProcessTrusted() != 0),
                screen_recording: crate::seam::permission(
                    "screen_recording",
                    CGPreflightScreenCaptureAccess(),
                ),
                post_event: crate::seam::permission("post_event", CGPreflightPostEventAccess()),
            }
        }
    }

    pub fn diagnostics(self) -> serde_json::Value {
        let executable = std::env::current_exe()
            .ok()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "manuvra-daemon".to_owned());
        serde_json::json!({
            "accessibility": self.accessibility,
            "screen_recording": self.screen_recording,
            "post_event": self.post_event,
            "responsible_identity": executable,
            "recovery": {
                "accessibility": "System Settings > Privacy & Security > Accessibility",
                "screen_recording": "System Settings > Privacy & Security > Screen & System Audio Recording",
                "post_event": "System Settings > Privacy & Security > Accessibility",
            },
            "prompts_triggered": false,
        })
    }

    pub(crate) fn missing_for(self, command: &str, foreground: bool) -> Option<MissingPermission> {
        if command == "observe.screenshot" {
            return (!self.screen_recording).then_some(MissingPermission::ScreenRecording);
        }
        if !self.accessibility {
            return Some(MissingPermission::Accessibility);
        }
        (foreground && command.starts_with("action.") && !self.post_event)
            .then_some(MissingPermission::PostEvent)
    }
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    // ApplicationServices declares these results as CoreFoundation `Boolean` (`unsigned char`),
    // not C99/Rust `bool`.
    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: Option<&CFDictionary<CFString, CFBoolean>>) -> u8;
    static kAXTrustedCheckOptionPrompt: Option<&'static CFString>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakePermissionApi {
        snapshots: Mutex<Vec<PermissionSnapshot>>,
        requested: Mutex<Vec<&'static str>>,
        opened: Mutex<Vec<&'static str>>,
        open_result: SetupResult,
    }

    #[derive(Clone, Copy)]
    enum SetupResult {
        Opened(bool),
        Error(SetupPermissionsError),
    }

    impl FakePermissionApi {
        fn new(before: PermissionSnapshot, after: PermissionSnapshot) -> Self {
            Self {
                snapshots: Mutex::new(vec![after, before]),
                requested: Mutex::new(Vec::new()),
                opened: Mutex::new(Vec::new()),
                open_result: SetupResult::Opened(true),
            }
        }

        fn with_open_result(mut self, result: SetupResult) -> Self {
            self.open_result = result;
            self
        }
    }

    impl PermissionApi for FakePermissionApi {
        fn snapshot(&self) -> PermissionSnapshot {
            self.snapshots.lock().unwrap().pop().unwrap()
        }

        fn request(&self, permission: Permission) {
            self.requested.lock().unwrap().push(permission.name());
        }

        fn open_settings(
            &self,
            url: &'static str,
            _deadline: Instant,
        ) -> Result<bool, SetupPermissionsError> {
            self.opened.lock().unwrap().push(url);
            match self.open_result {
                SetupResult::Opened(opened) => Ok(opened),
                SetupResult::Error(error) => Err(error),
            }
        }
    }

    #[test]
    fn permission_snapshot_serializes_named_independent_facts() {
        let value = serde_json::to_value(PermissionSnapshot {
            accessibility: true,
            screen_recording: false,
            post_event: true,
        })
        .unwrap();
        assert_eq!(value["accessibility"], true);
        assert_eq!(value["screen_recording"], false);
        assert_eq!(value["post_event"], true);
    }

    #[test]
    fn injected_permission_matrix_requires_only_the_native_primitive_in_use() {
        let all = PermissionSnapshot {
            accessibility: true,
            screen_recording: true,
            post_event: true,
        };
        assert_eq!(all.missing_for("observe.tree", false), None);
        assert_eq!(all.missing_for("action.click", true), None);

        let no_ax = PermissionSnapshot {
            accessibility: false,
            ..all
        };
        assert_eq!(
            no_ax.missing_for("observe.tree", false),
            Some(MissingPermission::Accessibility)
        );
        assert_eq!(no_ax.missing_for("observe.screenshot", false), None);

        let no_capture = PermissionSnapshot {
            screen_recording: false,
            ..all
        };
        assert_eq!(
            no_capture.missing_for("observe.screenshot", false),
            Some(MissingPermission::ScreenRecording)
        );
        assert_eq!(no_capture.missing_for("raw.ax.get", false), None);

        let no_post = PermissionSnapshot {
            post_event: false,
            ..all
        };
        assert_eq!(no_post.missing_for("action.click", false), None);
        assert_eq!(no_post.missing_for("observe.tree", true), None);
        assert_eq!(
            no_post.missing_for("action.click", true),
            Some(MissingPermission::PostEvent)
        );
        crate::test_oracles::write(
            "permission-matrix.json",
            &serde_json::json!({
                "schema": "manuvra/cp-07-boundary-oracle@1",
                "case": "permission_matrix",
                "rows": [
                    {"snapshot": "all", "command": "observe.tree", "foreground": false, "missing": null},
                    {"snapshot": "no_accessibility", "command": "observe.tree", "foreground": false, "missing": "Accessibility"},
                    {"snapshot": "no_accessibility", "command": "observe.screenshot", "foreground": false, "missing": null},
                    {"snapshot": "no_screen_recording", "command": "observe.screenshot", "foreground": false, "missing": "ScreenRecording"},
                    {"snapshot": "no_screen_recording", "command": "raw.ax.get", "foreground": false, "missing": null},
                    {"snapshot": "no_post_event", "command": "action.click", "foreground": false, "missing": null},
                    {"snapshot": "no_post_event", "command": "observe.tree", "foreground": true, "missing": null},
                    {"snapshot": "no_post_event", "command": "action.click", "foreground": true, "missing": "PostEvent"}
                ],
                "prompts_triggered": false
            }),
        );
    }

    #[test]
    fn setup_requests_only_false_preflights_and_rechecks_every_fact() {
        let api = FakePermissionApi::new(
            PermissionSnapshot {
                accessibility: false,
                screen_recording: true,
                post_event: false,
            },
            PermissionSnapshot {
                accessibility: false,
                screen_recording: true,
                post_event: false,
            },
        );

        let result =
            setup_permissions_with(&api, Instant::now() + std::time::Duration::from_secs(1))
                .unwrap();

        assert_eq!(
            *api.requested.lock().unwrap(),
            vec!["accessibility", "post_event"]
        );
        assert_eq!(
            result["permissions"]["accessibility"]["prompt_requested"],
            true
        );
        assert_eq!(
            result["permissions"]["accessibility"]["freshly_granted"],
            false
        );
        assert_eq!(result["permissions"]["accessibility"]["residual"], true);
        assert_eq!(
            result["permissions"]["accessibility"]["settings_opened"],
            true
        );
        assert_eq!(
            result["permissions"]["screen_recording"]["prompt_requested"],
            false
        );
        assert_eq!(result["permissions"]["screen_recording"]["granted"], true);
        assert_eq!(
            result["permissions"]["post_event"]["prompt_requested"],
            true
        );
        assert_eq!(
            result["permissions"]["post_event"]["freshly_granted"],
            false
        );
        assert_eq!(result["permissions"]["post_event"]["residual"], true);
        assert_eq!(result["permissions"]["post_event"]["settings_opened"], true);
        assert_eq!(
            *api.opened.lock().unwrap(),
            vec![ACCESSIBILITY_SETTINGS_URL],
            "Accessibility and Post Event share one pane"
        );
    }

    #[test]
    fn all_granted_setup_requests_nothing() {
        let granted = PermissionSnapshot {
            accessibility: true,
            screen_recording: true,
            post_event: true,
        };
        let api = FakePermissionApi::new(granted, granted);

        let result =
            setup_permissions_with(&api, Instant::now() + std::time::Duration::from_secs(1))
                .unwrap();

        assert!(api.requested.lock().unwrap().is_empty());
        assert!(api.opened.lock().unwrap().is_empty());
        assert!(PERMISSIONS.iter().all(|permission| {
            result["permissions"][permission.name()]["prompt_requested"] == false
                && result["permissions"][permission.name()]["granted"] == true
                && result["permissions"][permission.name()]["residual"] == false
        }));
    }

    #[test]
    fn expired_setup_requests_nothing() {
        let missing = PermissionSnapshot {
            accessibility: false,
            screen_recording: false,
            post_event: false,
        };
        let api = FakePermissionApi::new(missing, missing);

        assert!(setup_permissions_with(&api, Instant::now()).is_err());
        assert!(api.requested.lock().unwrap().is_empty());
        assert!(api.opened.lock().unwrap().is_empty());
    }

    #[test]
    fn settings_timeout_is_returned_without_replaying_an_open() {
        let missing = PermissionSnapshot {
            accessibility: false,
            screen_recording: true,
            post_event: true,
        };
        let api = FakePermissionApi::new(missing, missing)
            .with_open_result(SetupResult::Error(SetupPermissionsError::Deadline));

        assert_eq!(
            setup_permissions_with(&api, Instant::now() + Duration::from_secs(1)),
            Err(SetupPermissionsError::Deadline)
        );
        assert_eq!(
            *api.opened.lock().unwrap(),
            vec![ACCESSIBILITY_SETTINGS_URL]
        );
    }

    #[test]
    fn settings_failure_names_the_residual_permission() {
        let missing = PermissionSnapshot {
            accessibility: true,
            screen_recording: false,
            post_event: true,
        };
        let api =
            FakePermissionApi::new(missing, missing).with_open_result(SetupResult::Opened(false));

        assert_eq!(
            setup_permissions_with(&api, Instant::now() + Duration::from_secs(1)),
            Err(SetupPermissionsError::Settings("screen_recording"))
        );
    }
}
