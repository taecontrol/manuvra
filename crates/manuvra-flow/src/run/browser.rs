//! Launching the owned Chromium at the job's start URL, closing it, and the browser surfaces the
//! hosted loop drives.

use super::FlowConfig;
use super::capture::{BrowserPage, DriveBrowser};
use super::publish::unconfirmed_cleanup;
use manuvra_chrome::{BrowserConfig, BrowserError, CapturedPage, Observation, OwnedBrowser};
use manuvra_contract::{Cleanup, Job};
use serde_json::Value;

pub(super) struct StartedBrowser {
    pub(super) browser: OwnedBrowser,
    target_url: String,
}

pub(super) enum StartupFailure {
    Launch(BrowserError),
    AfterLaunch(Box<AfterLaunchFailure>),
}

pub(super) struct AfterLaunchFailure {
    pub(super) error: BrowserError,
    pub(super) provenance: Value,
    pub(super) cleanup: Cleanup,
}

impl StartedBrowser {
    pub(super) fn launch(config: BrowserConfig, target_url: &str) -> Result<Self, StartupFailure> {
        OwnedBrowser::launch(config)
            .map(|browser| Self {
                browser,
                target_url: target_url.into(),
            })
            .map_err(StartupFailure::Launch)
    }

    pub(super) fn provenance(&self) -> Value {
        serde_json::to_value(self.browser.provenance())
            .expect("browser provenance contains only serializable fields")
    }
    pub(super) fn navigate(mut self) -> Result<Self, StartupFailure> {
        self.browser
            .navigate(&self.target_url)
            .map_err(|error| self.after_launch_failure(error))?;
        Ok(self)
    }

    fn after_launch_failure(&mut self, error: BrowserError) -> StartupFailure {
        StartupFailure::AfterLaunch(Box::new(AfterLaunchFailure {
            error,
            provenance: self.provenance(),
            cleanup: cleanup_started_browser(&mut self.browser),
        }))
    }
}

pub(super) fn cleanup_started_browser(browser: &mut OwnedBrowser) -> Cleanup {
    if browser.close().is_ok() {
        Cleanup {
            browser: "closed".into(),
            profile: "removed".into(),
            application_state: "caller_owned".into(),
        }
    } else {
        unconfirmed_cleanup()
    }
}

pub(super) fn browser_config(job: &Job, config: &FlowConfig) -> BrowserConfig {
    let (width, height) = viewport(job);
    BrowserConfig {
        explicit_binary: config.browser.clone(),
        headless: config.headless,
        width,
        height,
        inherit_process_group: true,
    }
}

fn viewport(job: &Job) -> (u16, u16) {
    job.options
        .viewport
        .as_ref()
        .map_or((1120, 780), |value| (value.width, value.height))
}

pub(super) trait HostedBrowser: DriveBrowser {
    fn cleanup_hosted(&mut self) -> Cleanup;
}

impl HostedBrowser for OwnedBrowser {
    fn cleanup_hosted(&mut self) -> Cleanup {
        cleanup_started_browser(self)
    }
}

impl BrowserPage for OwnedBrowser {
    fn capture_redacted_matching_page(
        &self,
        sensitive: &[String],
        unchanged: &dyn Fn(&Observation, &Observation) -> bool,
    ) -> Result<CapturedPage, BrowserError> {
        self.capture_redacted_matching(sensitive, unchanged)
    }

    fn capture_redacted_page(&self, sensitive: &[String]) -> Result<CapturedPage, BrowserError> {
        self.capture_redacted(sensitive)
    }

    fn observe_page(&self) -> Result<Observation, BrowserError> {
        self.observe()
    }
}
