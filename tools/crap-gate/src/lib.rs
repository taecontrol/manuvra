use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

mod runner;

pub use runner::{GateConfig, run};

pub const ACCEPTED_THRESHOLD: f64 = 15.0;
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

#[derive(Debug, Clone, Deserialize)]
pub struct SwiftMetric {
    pub file: PathBuf,
    pub function: String,
    pub line: u64,
    pub end_line: u64,
    pub cyclomatic: f64,
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

pub type LineCoverage = HashMap<PathBuf, BTreeMap<u64, u64>>;

pub fn parse_lcov(path: &Path, repo_root: &Path) -> Result<LineCoverage> {
    let contents = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut result = LineCoverage::new();
    let mut current: Option<PathBuf> = None;
    for line in contents.lines() {
        if let Some(source) = line.strip_prefix("SF:") {
            let source = PathBuf::from(source);
            let source = if source.is_absolute() {
                source
            } else {
                repo_root.join(source)
            };
            current = Some(source.canonicalize().unwrap_or(source));
        } else if let Some(data) = line.strip_prefix("DA:") {
            let source = current.as_ref().context("LCOV DA record before SF")?;
            let mut fields = data.split(',');
            let line_number: u64 = fields.next().context("LCOV DA line missing")?.parse()?;
            let hits: u64 = fields.next().context("LCOV DA count missing")?.parse()?;
            *result
                .entry(source.clone())
                .or_default()
                .entry(line_number)
                .or_default() += hits;
        } else if line == "end_of_record" {
            current = None;
        }
    }
    Ok(result)
}

pub fn swift_entries(
    metrics: &[SwiftMetric],
    coverage: &LineCoverage,
    repo_root: &Path,
) -> Result<Vec<Entry>> {
    let mut by_file: HashMap<PathBuf, Vec<&SwiftMetric>> = HashMap::new();
    for metric in metrics {
        let file = metric
            .file
            .canonicalize()
            .unwrap_or_else(|_| metric.file.clone());
        by_file.entry(file).or_default().push(metric);
    }

    let mut entries = Vec::new();
    for (file, functions) in by_file {
        for (index, function) in functions.iter().enumerate() {
            if function.line > function.end_line {
                bail!("invalid Swift function span in {}", file.display());
            }
            let mut nested = Vec::new();
            for (other_index, other) in functions.iter().enumerate() {
                if index == other_index {
                    continue;
                }
                let overlaps = function.line <= other.end_line && other.line <= function.end_line;
                if !overlaps {
                    continue;
                }
                let strictly_nested =
                    function.line < other.line && other.end_line < function.end_line;
                let contains_function =
                    other.line < function.line && function.end_line < other.end_line;
                if strictly_nested {
                    nested.push((other.line, other.end_line));
                } else if !contains_function {
                    bail!(
                        "Swift function spans overlap ambiguously in {}: {}-{} and {}-{}",
                        file.display(),
                        function.line,
                        function.end_line,
                        other.line,
                        other.end_line
                    );
                }
            }

            let executable: Vec<u64> = coverage
                .get(&file)
                .into_iter()
                .flat_map(|lines| lines.iter())
                .filter(|(line, _)| {
                    function.line <= **line
                        && **line <= function.end_line
                        && !nested
                            .iter()
                            .any(|(start, end)| start <= *line && *line <= end)
                })
                .map(|(_, hits)| *hits)
                .collect();
            let missing = executable.is_empty();
            let coverage_percent = if missing {
                0.0
            } else {
                100.0 * executable.iter().filter(|hits| **hits > 0).count() as f64
                    / executable.len() as f64
            };
            entries.push(Entry {
                language: "swift".into(),
                file: relative_path(&file, repo_root),
                function: function.function.clone(),
                line: function.line,
                complexity: function.cyclomatic,
                coverage: coverage_percent,
                coverage_missing: missing,
                crap: crap_score(function.cyclomatic, coverage_percent),
            });
        }
    }
    Ok(entries)
}

pub fn compile_excludes(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).with_context(|| format!("invalid exclude glob {pattern}"))?);
    }
    Ok(builder.build()?)
}

pub fn swift_source_files(
    roots: &[PathBuf],
    repo_root: &Path,
    excludes: &GlobSet,
) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for root in roots {
        for item in WalkDir::new(root) {
            let item = item?;
            let path = item.path();
            if item.file_type().is_file() && path.extension().is_some_and(|ext| ext == "swift") {
                let display = relative_path(path, repo_root);
                if !excludes.is_match(&display) {
                    files.push(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
                }
            }
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
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
    use std::io::Write;

    #[test]
    fn formula_and_threshold_are_pinned() {
        assert_eq!(ACCEPTED_THRESHOLD, 15.0);
        assert_eq!(crap_score(1.0, 100.0), 1.0);
        assert_eq!(crap_score(4.0, 50.0), 6.0);
        assert_eq!(crap_score(6.0, 0.0), 42.0);
        assert!(!exceeds_threshold(15.0));
        assert!(exceeds_threshold(15.0001));
    }

    #[test]
    fn missing_swift_coverage_is_zero() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("Fixture.swift");
        fs::write(&source, "func f() {}\n").unwrap();
        let metrics = vec![SwiftMetric {
            file: source,
            function: "f".into(),
            line: 1,
            end_line: 1,
            cyclomatic: 3.0,
        }];
        let entries = swift_entries(&metrics, &LineCoverage::new(), root.path()).unwrap();
        assert_eq!(entries[0].coverage, 0.0);
        assert!(entries[0].coverage_missing);
        assert_eq!(entries[0].crap, 12.0);
    }

    #[test]
    fn lcov_is_bound_to_source_lines() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("Fixture.swift");
        fs::write(&source, "func f() {\n}\n").unwrap();
        let mut lcov = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            lcov,
            "SF:{}\nDA:1,1\nDA:2,0\nend_of_record",
            source.display()
        )
        .unwrap();
        let parsed = parse_lcov(lcov.path(), root.path()).unwrap();
        assert_eq!(parsed[&source.canonicalize().unwrap()][&1], 1);
        assert_eq!(parsed[&source.canonicalize().unwrap()][&2], 0);
    }

    #[test]
    fn nested_swift_lines_do_not_inflate_parent_coverage() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("Fixture.swift");
        fs::write(&source, "func outer() {\n { }\n}\n").unwrap();
        let source = source.canonicalize().unwrap();
        let metrics = vec![
            SwiftMetric {
                file: source.clone(),
                function: "outer".into(),
                line: 1,
                end_line: 3,
                cyclomatic: 1.0,
            },
            SwiftMetric {
                file: source.clone(),
                function: "closure".into(),
                line: 2,
                end_line: 2,
                cyclomatic: 1.0,
            },
        ];
        let mut lines = BTreeMap::new();
        lines.insert(1, 1);
        lines.insert(2, 1);
        lines.insert(3, 0);
        let coverage = HashMap::from([(source, lines)]);
        let entries = swift_entries(&metrics, &coverage, root.path()).unwrap();
        let outer = entries
            .iter()
            .find(|entry| entry.function == "outer")
            .unwrap();
        assert_eq!(outer.coverage, 50.0);
    }

    #[test]
    fn ambiguous_swift_spans_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("Fixture.swift");
        fs::write(&source, "func a() {}\n").unwrap();
        let metrics = vec![
            SwiftMetric {
                file: source.clone(),
                function: "a".into(),
                line: 1,
                end_line: 2,
                cyclomatic: 1.0,
            },
            SwiftMetric {
                file: source,
                function: "b".into(),
                line: 2,
                end_line: 3,
                cyclomatic: 1.0,
            },
        ];
        assert!(swift_entries(&metrics, &LineCoverage::new(), root.path()).is_err());
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

    #[test]
    fn swift_source_selection_honors_reviewed_globs() {
        let root = tempfile::tempdir().unwrap();
        let sources = root.path().join("Sources");
        let generated = sources.join("Generated");
        fs::create_dir_all(&generated).unwrap();
        let included = sources.join("Bridge.swift");
        fs::write(&included, "func bridge() {}\n").unwrap();
        fs::write(generated.join("Bindings.swift"), "func binding() {}\n").unwrap();
        fs::write(sources.join("README.md"), "not source\n").unwrap();

        let patterns = vec!["Sources/Generated/**".to_string()];
        let excludes = compile_excludes(&patterns).unwrap();
        let repo_root = root.path().canonicalize().unwrap();
        let selected = swift_source_files(&[sources], &repo_root, &excludes).unwrap();

        assert_eq!(selected, vec![included.canonicalize().unwrap()]);
    }
}
