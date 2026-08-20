use agent_manuvra_crap_gate::{ACCEPTED_THRESHOLD, Entry, Report, crap_score};
use anyhow::{Context, Result, bail};
use clap::Parser;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
#[command(about = "Verify a source-bound exhaustive CRAP certificate")]
struct Args {
    #[arg(long)]
    certificate: PathBuf,
    #[arg(long)]
    current_report: PathBuf,
}

fn main() {
    if let Err(error) = verify(Args::parse()) {
        eprintln!("CRAP certificate verification failed: {error:#}");
        std::process::exit(1);
    }
}

fn verify(args: Args) -> Result<()> {
    let certificate: Value = read_json(&args.certificate)?;
    let certified: Report = serde_json::from_value(
        certificate
            .get("crap_report")
            .cloned()
            .context("certificate has no crap_report")?,
    )
    .context("parse certified CRAP report")?;
    let current: Report = serde_json::from_value(read_json(&args.current_report)?)
        .context("parse current CRAP report")?;
    validate_report(&certified, true)?;
    validate_report(&current, false)?;
    compare_inventory(&certified.entries, &current.entries)?;
    let maximum = certified
        .entries
        .iter()
        .map(|entry| entry.crap)
        .fold(0.0_f64, f64::max);
    println!(
        "source-bound CRAP certificate: PASS; {}/{} above {}; maximum {:.2}",
        certified
            .entries
            .iter()
            .filter(|entry| entry.crap > ACCEPTED_THRESHOLD)
            .count(),
        certified.entries.len(),
        ACCEPTED_THRESHOLD,
        maximum
    );
    Ok(())
}

fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_slice(&fs::read(path).with_context(|| format!("read {}", path.display()))?)
        .with_context(|| format!("parse {}", path.display()))
}

fn validate_report(report: &Report, enforce_score: bool) -> Result<()> {
    if report.schema_version != 1 || report.threshold != ACCEPTED_THRESHOLD {
        bail!("unexpected CRAP report policy");
    }
    if report.entries.is_empty() {
        bail!("CRAP report has no production functions");
    }
    let mut identities = BTreeMap::new();
    for entry in &report.entries {
        validate_entry(entry, enforce_score)?;
        let identity = identity(entry);
        if identities.insert(identity.clone(), ()).is_some() {
            bail!("duplicate production function identity: {identity}");
        }
    }
    Ok(())
}

fn validate_entry(entry: &Entry, enforce_score: bool) -> Result<()> {
    if entry.language.is_empty()
        || entry.file.is_empty()
        || entry.file.starts_with('/')
        || entry.file.contains("..")
        || entry.function.is_empty()
        || entry.line == 0
        || !entry.complexity.is_finite()
        || entry.complexity < 1.0
        || !entry.coverage.is_finite()
        || !(0.0..=100.0).contains(&entry.coverage)
        || !entry.crap.is_finite()
    {
        bail!("invalid CRAP entry: {}", identity(entry));
    }
    if enforce_score {
        if entry.coverage_missing && entry.coverage != 0.0 {
            bail!("missing coverage was not treated pessimistically: {}", identity(entry));
        }
        let computed = crap_score(entry.complexity, entry.coverage);
        if (computed - entry.crap).abs() > 1e-6 {
            bail!("CRAP formula mismatch: {}", identity(entry));
        }
        if entry.crap > ACCEPTED_THRESHOLD {
            bail!("CRAP threshold exceeded: {}", identity(entry));
        }
    }
    Ok(())
}

fn compare_inventory(certified: &[Entry], current: &[Entry]) -> Result<()> {
    let certified = inventory(certified)?;
    let current = inventory(current)?;
    if certified.len() != current.len() {
        bail!(
            "production function count changed: certified {}, current {}",
            certified.len(),
            current.len()
        );
    }
    for (identity, certified_complexity) in &certified {
        let current_complexity = current
            .get(identity)
            .with_context(|| format!("certified function is absent from current source: {identity}"))?;
        if (certified_complexity - current_complexity).abs() > f64::EPSILON {
            bail!("complexity changed for {identity}");
        }
    }
    for identity in current.keys() {
        if !certified.contains_key(identity) {
            bail!("current source has an uncertified function: {identity}");
        }
    }
    Ok(())
}

fn inventory(entries: &[Entry]) -> Result<BTreeMap<String, f64>> {
    let mut inventory = BTreeMap::new();
    for entry in entries {
        let identity = identity(entry);
        if inventory.insert(identity.clone(), entry.complexity).is_some() {
            bail!("duplicate production function identity: {identity}");
        }
    }
    Ok(inventory)
}

fn identity(entry: &Entry) -> String {
    format!(
        "{}:{}:{}:{}",
        entry.language, entry.file, entry.line, entry.function
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(function: &str, complexity: f64, coverage: f64) -> Entry {
        Entry {
            language: "rust".into(),
            file: "crates/example/src/lib.rs".into(),
            function: function.into(),
            line: 1,
            complexity,
            coverage,
            coverage_missing: false,
            crap: crap_score(complexity, coverage),
        }
    }

    #[test]
    fn certified_scores_and_current_inventory_must_agree() {
        let certified = vec![entry("covered", 4.0, 100.0)];
        let current = vec![entry("covered", 4.0, 0.0)];
        validate_report(
            &Report {
                schema_version: 1,
                threshold: 15.0,
                entries: certified.clone(),
            },
            true,
        )
        .unwrap();
        compare_inventory(&certified, &current).unwrap();
        let changed = vec![entry("changed", 4.0, 0.0)];
        assert!(compare_inventory(&certified, &changed).is_err());
    }

    #[test]
    fn missing_coverage_is_pessimistic_and_threshold_is_strict() {
        let mut missing = entry("missing", 3.0, 50.0);
        missing.coverage_missing = true;
        assert!(validate_entry(&missing, true).is_err());
        missing.coverage = 0.0;
        missing.crap = crap_score(3.0, 0.0);
        validate_entry(&missing, true).unwrap();
        let above = entry("above", 5.0, 0.0);
        assert!(validate_entry(&above, true).is_err());
    }
}
