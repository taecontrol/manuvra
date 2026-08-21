use crate::config_root;
use manuvra_chrome::{
    Endpoint, GOOGLE_CHROME_MACOS, LaunchError, LaunchRequest, launch_dedicated_chrome,
};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

pub fn chrome_launch(timeout: Duration) -> Result<Value, LaunchError> {
    let endpoint = launch_endpoint()?;
    launch_dedicated_chrome(LaunchRequest {
        endpoint,
        profile: dedicated_profile()?,
        binary: GOOGLE_CHROME_MACOS.into(),
        timeout,
    })
}

fn dedicated_profile() -> Result<PathBuf, LaunchError> {
    let profile = config_root().join("chrome-dedicated");
    if profile.is_absolute() {
        return Ok(profile);
    }
    std::path::absolute(&profile).map_err(|error| {
        LaunchError::Unavailable(format!(
            "dedicated Chrome profile is not an absolute path: {error}"
        ))
    })
}

fn launch_endpoint() -> Result<Endpoint, LaunchError> {
    let configured = std::env::var("MANUVRA_CHROME_ENDPOINTS").ok();
    Endpoint::configured(configured.as_deref())
        .map_err(|error| LaunchError::InvalidEndpoint(error.to_string()))?
        .into_iter()
        .next()
        .ok_or_else(|| LaunchError::InvalidEndpoint("no Chrome endpoint configured".to_owned()))
}
