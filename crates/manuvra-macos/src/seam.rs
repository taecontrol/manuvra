#[cfg(debug_assertions)]
use serde_json::Value;

#[cfg(debug_assertions)]
fn config() -> Option<Value> {
    parse_config(std::env::var_os("MANUVRA_CP07_SEAM_PATH")?)
}

#[cfg(debug_assertions)]
fn parse_config(path: std::ffi::OsString) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

#[cfg(debug_assertions)]
pub(crate) fn permission(name: &str, actual: bool) -> bool {
    configured_permission(name).unwrap_or(actual)
}

#[cfg(debug_assertions)]
fn configured_permission(name: &str) -> Option<bool> {
    permissions()?.get(name).and_then(Value::as_bool)
}

#[cfg(debug_assertions)]
fn permissions() -> Option<Value> {
    config()?.get("permissions").cloned()
}

#[cfg(not(debug_assertions))]
pub(crate) fn permission(_name: &str, actual: bool) -> bool {
    actual
}

#[cfg(debug_assertions)]
pub(crate) fn journal_limit(kind: &str, default: usize) -> usize {
    configured_journal_limit(kind).unwrap_or(default)
}

#[cfg(debug_assertions)]
fn configured_journal_limit(kind: &str) -> Option<usize> {
    positive_limit(journal_limit_u64(kind)?)
}

#[cfg(debug_assertions)]
fn journal_limit_u64(kind: &str) -> Option<u64> {
    journal_limits()?.get(kind).and_then(Value::as_u64)
}

#[cfg(debug_assertions)]
fn journal_limits() -> Option<Value> {
    config()?.get("journal_limits").cloned()
}

#[cfg(debug_assertions)]
fn positive_limit(limit: u64) -> Option<usize> {
    usize::try_from(limit).ok().filter(|limit| *limit > 0)
}

#[cfg(not(debug_assertions))]
pub(crate) fn journal_limit(_kind: &str, default: usize) -> usize {
    default
}

#[cfg(debug_assertions)]
pub(crate) fn frame_scale(actual: f64) -> f64 {
    configured_frame_scale().unwrap_or(actual)
}

#[cfg(debug_assertions)]
fn configured_frame_scale() -> Option<f64> {
    finite_positive(frame_scale_value()?)
}

#[cfg(debug_assertions)]
fn frame_scale_value() -> Option<f64> {
    config()?.get("frame_scale").and_then(Value::as_f64)
}

#[cfg(debug_assertions)]
fn finite_positive(scale: f64) -> Option<f64> {
    (scale.is_finite() && scale > 0.0).then_some(scale)
}

#[cfg(not(debug_assertions))]
pub(crate) fn frame_scale(actual: f64) -> f64 {
    actual
}

#[cfg(test)]
mod tests {
    #[test]
    fn seam_defaults_without_cp07_config() {
        assert!(super::permission("accessibility", true));
        assert!(!super::permission("accessibility", false));
        assert_eq!(super::journal_limit("logs", 256), 256);
        assert_eq!(super::frame_scale(2.0), 2.0);
        assert!(super::config().is_none());
        assert!(super::finite_positive(2.0).is_some());
        assert!(super::finite_positive(0.0).is_none());
        assert!(super::positive_limit(8).is_some());
        assert!(super::positive_limit(0).is_none());
    }
}
