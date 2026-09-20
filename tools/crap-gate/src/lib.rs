use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

mod runner;

pub use runner::{GateConfig, run};

pub const ACCEPTED_THRESHOLD: f64 = 8.0;
pub const EXIT_GATE_FAILED: i32 = 1;
pub const EXIT_ANALYSIS_FAILED: i32 = 2;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Entry {
    pub language: String,
    pub file: String,
    pub function: String,
    pub line: u64,
    pub complexity: f64,
    pub coverage: f64,
    pub coverage_missing: bool,
    pub crap: f64,
}

#[derive(Debug, Deserialize)]
pub struct CargoCrapEnvelope {
    pub entries: Vec<CargoCrapEntry>,
}

#[derive(Debug, Deserialize)]
pub struct CargoCrapEntry {
    pub file: PathBuf,
    pub function: String,
    pub line: u64,
    pub cyclomatic: f64,
    pub coverage: Option<f64>,
    pub crap: f64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub threshold: f64,
    pub entries: Vec<Entry>,
}

pub fn crap_score(complexity: f64, coverage: f64) -> f64 {
    complexity.powi(2) * (1.0 - coverage / 100.0).powi(3) + complexity
}

pub fn exceeds_threshold(score: f64) -> bool {
    score > ACCEPTED_THRESHOLD
}

pub fn relative_path(path: &Path, repo_root: &Path) -> String {
    let normalized = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    normalized
        .strip_prefix(repo_root)
        .unwrap_or(&normalized)
        .to_string_lossy()
        .replace('\\', "/")
}

pub fn rust_entries(report_path: &Path, rust_root: &Path, repo_root: &Path) -> Result<Vec<Entry>> {
    let report: CargoCrapEnvelope = serde_json::from_slice(
        &fs::read(report_path).with_context(|| format!("read {}", report_path.display()))?,
    )
    .context("parse cargo-crap JSON")?;
    Ok(report
        .entries
        .into_iter()
        .map(|entry| {
            let source = if entry.file.is_absolute() {
                entry.file
            } else {
                let repo_candidate = repo_root.join(&entry.file);
                if repo_candidate.exists() {
                    repo_candidate
                } else {
                    rust_root.join(entry.file)
                }
            };
            Entry {
                language: "rust".into(),
                file: relative_path(&source, repo_root),
                function: entry.function,
                line: entry.line,
                complexity: entry.cyclomatic,
                coverage: entry.coverage.unwrap_or(0.0),
                coverage_missing: entry.coverage.is_none(),
                crap: entry.crap,
            }
        })
        .collect())
}

pub fn compact_report(entries: &[Entry], top: usize) -> String {
    let mut offenders: Vec<&Entry> = entries
        .iter()
        .filter(|entry| exceeds_threshold(entry.crap))
        .collect();
    offenders.sort_by(|left, right| {
        right
            .crap
            .total_cmp(&left.crap)
            .then_with(|| left.file.cmp(&right.file))
            .then_with(|| left.line.cmp(&right.line))
    });
    let mut lines = vec![format!(
        "CRAP gate: {}; {}/{} function(s) above {}",
        if offenders.is_empty() { "PASS" } else { "FAIL" },
        offenders.len(),
        entries.len(),
        ACCEPTED_THRESHOLD as u64
    )];
    for entry in offenders.iter().take(top) {
        let missing = if entry.coverage_missing {
            " (missing->0%)"
        } else {
            ""
        };
        lines.push(format!(
            "  {:.2}  CC={:.0}  cov={:.1}%{}  {}:{}:{} {}",
            entry.crap,
            entry.complexity,
            entry.coverage,
            missing,
            entry.language,
            entry.file,
            entry.line,
            entry.function
        ));
    }
    if offenders.len() > top {
        lines.push(format!("  ... {} more offender(s)", offenders.len() - top));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formula_and_threshold_are_pinned() {
        assert_eq!(ACCEPTED_THRESHOLD, 8.0);
        assert_eq!(crap_score(1.0, 100.0), 1.0);
        assert_eq!(crap_score(4.0, 50.0), 6.0);
        assert_eq!(crap_score(6.0, 0.0), 42.0);
        assert!(!exceeds_threshold(8.0));
        assert!(exceeds_threshold(8.0001));
    }

    #[test]
    fn report_is_bounded() {
        let entry = |name: &str, crap: f64| Entry {
            language: "rust".into(),
            file: "src/lib.rs".into(),
            function: name.into(),
            line: 1,
            complexity: 6.0,
            coverage: 0.0,
            coverage_missing: true,
            crap,
        };
        let output = compact_report(&[entry("worst", 42.0), entry("second", 30.0)], 1);
        assert!(output.contains("worst"));
        assert!(!output.contains("second"));
        assert!(output.contains("1 more offender"));
        assert!(output.contains("missing->0%"));
    }
}
