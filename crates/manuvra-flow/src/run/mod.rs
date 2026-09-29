//! One run of a job against one owned Chromium: the hosted loop, autonomous step evaluation,
//! caller dispositions, final verification, and the evidence the run publishes.

#[cfg(any(target_os = "linux", target_os = "macos", test))]
mod artifacts;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod browser;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
mod capture;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
mod disposition;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
mod escalation;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
mod execute;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
mod final_verification;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod hosted;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
mod machine;
mod publish;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
mod step_driver;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
mod stops;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use hosted::run_hosted;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use crate::evidence::Redactor;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use manuvra_contract::DispositionRequest;
use manuvra_contract::Job;
use serde_json::Value;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone)]
pub struct FlowConfig {
    pub request_id: String,
    pub run_id: String,
    pub evidence_root: PathBuf,
    pub browser: Option<PathBuf>,
    pub headless: bool,
}

pub struct FlowOutcome {
    pub result: Value,
    pub exit_code: u8,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostedTermination {
    Aborted,
    PauseDeadlineElapsed,
    LifetimeElapsed,
    WatchdogLost,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Debug, Clone)]
pub enum HostedEvent {
    Disposition(DispositionRequest),
    Termination(HostedTermination),
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub trait HostedControl {
    fn cancellation(&self) -> manuvra_chrome::InputCancellation;
    fn pause_deadline_unix_ms(&self) -> u64;
    fn publish_checkpoint(&self, result: &Value) -> Result<(), String>;
    fn wait_while_paused(&self, escalation_id: &str) -> HostedEvent;
    fn termination(&self) -> Option<HostedTermination>;
}

/// Publishes the honest `unsupported_platform` run on a platform without a supported runtime.
/// Supported platforms run every job through [`run_hosted`].
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn run(job: &Job, config: FlowConfig, redactor: &Redactor) -> Result<FlowOutcome, String> {
    publish::publish_without_browser(
        job,
        config,
        redactor,
        "unsupported_platform",
        BTreeMap::new(),
    )
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn target_url(job: &Job) -> &str {
    match &job.target {
        manuvra_contract::Target::Browser { url } => url,
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(super) mod live;
    pub(super) mod support;

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn unsupported_platform_publishes_a_complete_blocked_run() {
        use super::*;
        use support::job;
        use tempfile::TempDir;

        let job = job("Ready");
        let redactor = Redactor::for_job(&job).unwrap();
        let temp = TempDir::new().unwrap();
        let outcome = run(
            &job,
            FlowConfig {
                request_id: "unsupported".into(),
                run_id: "r_unsupported".into(),
                evidence_root: temp.path().to_path_buf(),
                browser: None,
                headless: true,
            },
            &redactor,
        )
        .unwrap();
        assert_eq!(outcome.exit_code, 3);
        assert_eq!(outcome.result["reason"]["code"], "unsupported_platform");
        assert!(temp.path().join("r_unsupported/manifest.json").is_file());
    }
}
