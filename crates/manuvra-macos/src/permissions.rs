use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct PermissionSnapshot {
    pub accessibility: bool,
    pub screen_recording: bool,
    pub post_event: bool,
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
                accessibility: crate::seam::permission("accessibility", AXIsProcessTrusted()),
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
    fn AXIsProcessTrusted() -> bool;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGPreflightPostEventAccess() -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
