#[cfg(debug_assertions)]
use serde_json::Value;

#[cfg(debug_assertions)]
fn config() -> Option<Value> {
    let path = std::env::var_os("MANUVRA_CP07_SEAM_PATH")?;
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(debug_assertions)]
pub(crate) fn permission(name: &str, actual: bool) -> bool {
    config()
        .and_then(|config| config.get("permissions")?.get(name)?.as_bool())
        .unwrap_or(actual)
}

#[cfg(not(debug_assertions))]
pub(crate) fn permission(_name: &str, actual: bool) -> bool {
    actual
}

#[cfg(debug_assertions)]
pub(crate) fn journal_limit(kind: &str, default: usize) -> usize {
    config()
        .and_then(|config| config.get("journal_limits")?.get(kind)?.as_u64())
        .and_then(|limit| usize::try_from(limit).ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(default)
}

#[cfg(not(debug_assertions))]
pub(crate) fn journal_limit(_kind: &str, default: usize) -> usize {
    default
}

#[cfg(debug_assertions)]
pub(crate) fn frame_scale(actual: f64) -> f64 {
    config()
        .and_then(|config| config.get("frame_scale")?.as_f64())
        .filter(|scale| scale.is_finite() && *scale > 0.0)
        .unwrap_or(actual)
}

#[cfg(not(debug_assertions))]
pub(crate) fn frame_scale(actual: f64) -> f64 {
    actual
}
