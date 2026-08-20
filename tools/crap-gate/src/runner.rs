use crate::{
    ACCEPTED_THRESHOLD, EXIT_ANALYSIS_FAILED, EXIT_GATE_FAILED, Report, SwiftMetric,
    compact_report, compile_excludes, exceeds_threshold, parse_lcov, rust_entries, swift_entries,
    swift_source_files,
};
use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;
use walkdir::WalkDir;

const EXPECTED_CARGO_CRAP: &str = "0.4.3";
const EXPECTED_CARGO_LLVM_COV: &str = "0.9.0";

#[derive(Debug, Clone)]
pub struct GateConfig {
    pub top: usize,
    pub repo_root: PathBuf,
    pub rust_manifest: PathBuf,
    pub rust_root: PathBuf,
    pub swift_package: Option<PathBuf>,
    pub swift_root: Vec<PathBuf>,
    pub exclude: Vec<String>,
    pub rust_coverage_ignore_regex: Option<String>,
    pub report_json: Option<PathBuf>,
    pub cargo_crap: String,
    pub cargo_llvm_cov: String,
    pub swift: String,
    pub swift_analyzer_package: PathBuf,
    pub llvm_cov: Option<PathBuf>,
    pub llvm_profdata: Option<PathBuf>,
}

struct Runtime {
    repo_root: PathBuf,
    llvm_cov: PathBuf,
    llvm_profdata: PathBuf,
    temporary: TempDir,
}

pub fn run(config: GateConfig) -> i32 {
    match execute(&config) {
        Ok(true) => EXIT_GATE_FAILED,
        Ok(false) => 0,
        Err(error) => {
            eprintln!("CRAP analysis error: {error:#}");
            EXIT_ANALYSIS_FAILED
        }
    }
}

fn execute(config: &GateConfig) -> Result<bool> {
    validate_config(config)?;
    execute_valid(config)
}

fn validate_config(config: &GateConfig) -> Result<()> {
    if config.top == 0 {
        bail!("--top must be positive");
    }
    if config.swift_package.is_some() == config.swift_root.is_empty() {
        bail!("--swift-package and --swift-root must be provided together");
    }
    Ok(())
}

fn execute_valid(config: &GateConfig) -> Result<bool> {
    Runtime::new(config).and_then(|runtime| {
        measurement_entries(config, &runtime).and_then(|entries| conclude(config, entries))
    })
}

impl Runtime {
    fn new(config: &GateConfig) -> Result<Self> {
        let repo_root = config
            .repo_root
            .canonicalize()
            .context("canonicalize repo root")?;
        let temporary = tempfile::Builder::new()
            .prefix("agent-manuvra-crap-")
            .tempdir()?;
        let llvm_cov = resolve_tool(config.llvm_cov.clone(), "llvm-cov")?;
        let llvm_profdata = resolve_tool(config.llvm_profdata.clone(), "llvm-profdata")?;
        Ok(Self {
            repo_root,
            llvm_cov,
            llvm_profdata,
            temporary,
        })
    }
}

fn resolve_tool(explicit: Option<PathBuf>, tool: &str) -> Result<PathBuf> {
    explicit.map(Ok).unwrap_or_else(|| xcrun_find(tool))
}

fn measurement_entries(config: &GateConfig, runtime: &Runtime) -> Result<Vec<crate::Entry>> {
    rust_measurements(config, runtime).and_then(|mut entries| {
        add_swift_measurements(config, runtime, &mut entries).map(|()| entries)
    })
}

fn rust_measurements(config: &GateConfig, runtime: &Runtime) -> Result<Vec<crate::Entry>> {
    generate_rust_lcov(config, runtime).and_then(|lcov| {
        generate_cargo_crap_report(config, &lcov, runtime)
            .and_then(|report| rust_entries(&report, &config.rust_root, &runtime.repo_root))
    })
}

fn add_swift_measurements(
    config: &GateConfig,
    runtime: &Runtime,
    entries: &mut Vec<crate::Entry>,
) -> Result<()> {
    if config.swift_package.is_none() {
        return Ok(());
    }
    entries.extend(swift_measurements(config, runtime)?);
    Ok(())
}

fn swift_measurements(config: &GateConfig, runtime: &Runtime) -> Result<Vec<crate::Entry>> {
    generate_swift_lcov(config, runtime).and_then(|lcov| {
        parse_lcov(&lcov, &runtime.repo_root).and_then(|coverage| {
            swift_metrics(config, runtime)
                .and_then(|metrics| swift_entries(&metrics, &coverage, &runtime.repo_root))
        })
    })
}

fn conclude(config: &GateConfig, mut entries: Vec<crate::Entry>) -> Result<bool> {
    ensure_entries(&entries)?;
    entries.sort_by(|left, right| {
        left.file
            .cmp(&right.file)
            .then_with(|| left.line.cmp(&right.line))
    });
    println!("{}", compact_report(&entries, config.top));
    write_json_report(config.report_json.as_deref(), &entries)?;
    Ok(entries.iter().any(|entry| exceeds_threshold(entry.crap)))
}

fn ensure_entries(entries: &[crate::Entry]) -> Result<()> {
    if entries.is_empty() {
        bail!("no production functions were analyzed");
    }
    Ok(())
}

fn write_json_report(path: Option<&Path>, entries: &[crate::Entry]) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    let report = Report {
        schema_version: 1,
        threshold: ACCEPTED_THRESHOLD,
        entries: entries.to_vec(),
    };
    let json = serde_json::to_string_pretty(&report)?;
    fs::write(path, format!("{json}\n"))?;
    Ok(())
}

fn checked_output(command: &mut Command, label: &str) -> Result<Output> {
    let output = command.output().with_context(|| format!("run {label}"))?;
    if !output.status.success() {
        bail!(
            "{label} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output)
}

fn checked_status(command: &mut Command, label: &str) -> Result<()> {
    let status = command.status().with_context(|| format!("run {label}"))?;
    if !status.success() {
        bail!("{label} failed with {status}");
    }
    Ok(())
}

fn version(command: &str, args: &[&str]) -> Result<String> {
    let output = checked_output(Command::new(command).args(args), command)?;
    String::from_utf8(output.stdout)?
        .split_whitespace()
        .last()
        .map(ToOwned::to_owned)
        .context("version output was empty")
}

fn require_version(command: &str, args: &[&str], expected: &str) -> Result<()> {
    let found = version(command, args)?;
    if found != expected {
        bail!("{command} must be {expected}; got {found}");
    }
    Ok(())
}

fn xcrun_find(tool: &str) -> Result<PathBuf> {
    let output = checked_output(Command::new("xcrun").args(["--find", tool]), "xcrun")?;
    Ok(PathBuf::from(String::from_utf8(output.stdout)?.trim()))
}

fn generate_rust_lcov(config: &GateConfig, runtime: &Runtime) -> Result<PathBuf> {
    require_version(
        &config.cargo_llvm_cov,
        &["llvm-cov", "--version"],
        EXPECTED_CARGO_LLVM_COV,
    )?;
    let lcov = runtime.temporary.path().join("rust.lcov");
    let environment = rust_coverage_environment(config, runtime)?;
    let mut test = rust_test_command(config, runtime, &environment);
    checked_status(&mut test, "instrumented cargo test")?;
    let profiles = rust_coverage_profiles(runtime)?;
    let merged = runtime.temporary.path().join("rust.profdata");
    let mut merge = rust_profile_merge_command(runtime, &profiles, &merged);
    checked_status(&mut merge, "llvm-profdata merge")?;
    let objects = rust_coverage_objects(runtime)?;
    let mut export = rust_coverage_export_command(config, runtime, &merged, &objects);
    let output = checked_output(&mut export, "llvm-cov export")?;
    fs::write(&lcov, output.stdout)?;
    Ok(lcov)
}

fn rust_coverage_environment(
    config: &GateConfig,
    runtime: &Runtime,
) -> Result<Vec<(String, String)>> {
    let mut command = rust_show_env_command(config, runtime);
    let output = checked_output(&mut command, "cargo llvm-cov show-env")?;
    parse_shell_exports(&String::from_utf8(output.stdout)?)
}

fn rust_show_env_command(config: &GateConfig, runtime: &Runtime) -> Command {
    let mut command = Command::new(&config.cargo_llvm_cov);
    command
        .args(["llvm-cov", "show-env", "--sh", "--manifest-path"])
        .arg(&config.rust_manifest)
        .env(
            "CARGO_TARGET_DIR",
            runtime.temporary.path().join("cargo-target"),
        )
        .env("LLVM_COV", &runtime.llvm_cov)
        .env("LLVM_PROFDATA", &runtime.llvm_profdata)
        .current_dir(&runtime.repo_root);
    command
}

fn rust_test_command(
    config: &GateConfig,
    runtime: &Runtime,
    environment: &[(String, String)],
) -> Command {
    let mut command = Command::new("cargo");
    command
        .args(["test", "--workspace", "--tests", "--all-features", "--locked"])
        .args(["--manifest-path"])
        .arg(&config.rust_manifest)
        .envs(environment.iter().map(|(key, value)| (key, value)))
        .env(
            "CARGO_TARGET_DIR",
            runtime.temporary.path().join("cargo-target"),
        )
        .current_dir(&runtime.repo_root);
    command
}

fn rust_profile_merge_command(runtime: &Runtime, profiles: &[PathBuf], output: &Path) -> Command {
    let mut command = Command::new(&runtime.llvm_profdata);
    command.arg("merge").arg("-sparse").args(profiles).arg("-o").arg(output);
    command
}

fn rust_coverage_export_command(
    config: &GateConfig,
    runtime: &Runtime,
    profile: &Path,
    objects: &[PathBuf],
) -> Command {
    let mut command = Command::new(&runtime.llvm_cov);
    command
        .args(["export", "-format=lcov", "-instr-profile"])
        .arg(profile)
        .arg(&objects[0])
        .current_dir(&runtime.repo_root);
    for object in &objects[1..] {
        command.arg("--object").arg(object);
    }
    add_coverage_ignore(&mut command, config.rust_coverage_ignore_regex.as_deref());
    command
}

fn rust_coverage_objects(runtime: &Runtime) -> Result<Vec<PathBuf>> {
    let directory = runtime.temporary.path().join("cargo-target/debug");
    let mut objects = WalkDir::new(&directory)
        .into_iter()
        .filter_map(Result::ok)
        .map(|entry| entry.into_path())
        .filter(|path| {
            path.extension().is_none()
                && path.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .collect::<Vec<_>>();
    objects.sort();
    if objects.is_empty() {
        bail!("instrumented cargo test produced no executable coverage objects");
    }
    Ok(objects)
}

fn rust_coverage_profiles(runtime: &Runtime) -> Result<Vec<PathBuf>> {
    let directory = runtime.temporary.path().join("cargo-target");
    let mut profiles = WalkDir::new(&directory)
        .into_iter()
        .filter_map(Result::ok)
        .map(|entry| entry.into_path())
        .filter(|path| path.extension() == Some(OsStr::new("profraw")))
        .collect::<Vec<_>>();
    profiles.sort();
    if profiles.is_empty() {
        bail!("instrumented cargo test produced no raw coverage profiles");
    }
    Ok(profiles)
}

fn parse_shell_exports(output: &str) -> Result<Vec<(String, String)>> {
    let exports = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let assignment = line
                .strip_prefix("export ")
                .context("cargo llvm-cov show-env emitted a non-export line")?;
            let (key, quoted) = assignment
                .split_once('=')
                .context("cargo llvm-cov show-env emitted an invalid assignment")?;
            let value = quoted
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
                .unwrap_or(quoted);
            Ok((key.to_owned(), value.to_owned()))
        })
        .collect::<Result<Vec<_>>>()?;
    if exports.is_empty() {
        bail!("cargo llvm-cov show-env emitted no environment variables");
    }
    Ok(exports)
}

fn add_coverage_ignore(command: &mut Command, pattern: Option<&str>) {
    if let Some(pattern) = pattern {
        command.args(["--ignore-filename-regex", pattern]);
    }
}

fn generate_cargo_crap_report(
    config: &GateConfig,
    lcov: &Path,
    runtime: &Runtime,
) -> Result<PathBuf> {
    require_version(
        &config.cargo_crap,
        &["crap", "--version"],
        EXPECTED_CARGO_CRAP,
    )?;
    let report = runtime.temporary.path().join("rust.json");
    let mut command = cargo_crap_command(config, lcov, &report);
    checked_status(&mut command, "cargo-crap")?;
    Ok(report)
}

fn cargo_crap_command(config: &GateConfig, lcov: &Path, report: &Path) -> Command {
    let mut command = Command::new(&config.cargo_crap);
    command
        .args(["crap", "--path"])
        .arg(&config.rust_root)
        .arg("--lcov")
        .arg(lcov)
        .args([
            "--missing",
            "pessimistic",
            "--format",
            "json",
            "--sort",
            "file",
            "--output",
        ])
        .arg(report);
    add_excludes(&mut command, &config.exclude);
    command
}

fn add_excludes(command: &mut Command, patterns: &[String]) {
    for pattern in patterns {
        command.args(["--exclude", pattern]);
    }
}

fn swift_environment(command: &mut Command, temporary: &TempDir) {
    let cache = temporary.path().join("swift-module-cache");
    command
        .env("SWIFTPM_MODULECACHE_OVERRIDE", &cache)
        .env("CLANG_MODULE_CACHE_PATH", cache);
}

fn is_package_test_binary(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.ends_with("PackageTests"))
        && path.to_string_lossy().contains(".xctest/Contents/MacOS/")
        && !path.to_string_lossy().contains(".dSYM/")
}

fn package_test_binary(scratch: &Path) -> Result<PathBuf> {
    let candidates = package_test_binary_candidates(scratch)?;
    exactly_one_binary(candidates)
}

fn package_test_binary_candidates(scratch: &Path) -> Result<Vec<PathBuf>> {
    WalkDir::new(scratch)
        .into_iter()
        .map(|item| item.map_err(anyhow::Error::from))
        .filter_map(|item| match item {
            Ok(item) if item.file_type().is_file() && is_package_test_binary(item.path()) => {
                Some(Ok(item.path().to_path_buf()))
            }
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn exactly_one_binary(mut candidates: Vec<PathBuf>) -> Result<PathBuf> {
    if candidates.len() != 1 {
        bail!(
            "expected one SwiftPM PackageTests binary; found {}",
            candidates.len()
        );
    }
    Ok(candidates.remove(0))
}

fn generate_swift_lcov(config: &GateConfig, runtime: &Runtime) -> Result<PathBuf> {
    run_swift_coverage(config, runtime)?;
    swift_profile(config, runtime).and_then(|profile| export_swift_lcov(runtime, &profile))
}

fn swift_package(config: &GateConfig) -> Result<&Path> {
    config
        .swift_package
        .as_deref()
        .context("Swift package missing")
}

fn swift_scratch(runtime: &Runtime) -> PathBuf {
    runtime.temporary.path().join("swift-build")
}

fn run_swift_coverage(config: &GateConfig, runtime: &Runtime) -> Result<()> {
    let mut command = Command::new(&config.swift);
    command
        .args(["test", "--package-path"])
        .arg(swift_package(config)?)
        .arg("--scratch-path")
        .arg(swift_scratch(runtime))
        .arg("--enable-code-coverage");
    swift_environment(&mut command, &runtime.temporary);
    checked_status(&mut command, "swift test coverage")
}

fn swift_profile(config: &GateConfig, runtime: &Runtime) -> Result<PathBuf> {
    swift_codecov_path(config, runtime).and_then(|path| {
        path.parent()
            .map(|parent| parent.join("default.profdata"))
            .context("codecov path has no parent")
    })
}

fn swift_codecov_path(config: &GateConfig, runtime: &Runtime) -> Result<PathBuf> {
    let mut command = Command::new(&config.swift);
    command
        .args(["test", "--package-path"])
        .arg(swift_package(config)?)
        .arg("--scratch-path")
        .arg(swift_scratch(runtime))
        .arg("--show-codecov-path");
    swift_environment(&mut command, &runtime.temporary);
    checked_output(&mut command, "swift codecov path").and_then(output_path)
}

fn output_path(output: Output) -> Result<PathBuf> {
    Ok(PathBuf::from(String::from_utf8(output.stdout)?.trim()))
}

fn export_swift_lcov(runtime: &Runtime, profile: &Path) -> Result<PathBuf> {
    package_test_binary(&swift_scratch(runtime)).and_then(|binary| {
        llvm_cov_export(&runtime.llvm_cov, profile, &binary)
            .and_then(|stdout| persist_swift_lcov(runtime, &stdout))
    })
}

fn llvm_cov_export(llvm_cov: &Path, profile: &Path, binary: &Path) -> Result<Vec<u8>> {
    let output = checked_output(
        Command::new(llvm_cov)
            .args(["export", "-format=lcov"])
            .arg(format!("-instr-profile={}", profile.display()))
            .arg(binary),
        "llvm-cov Swift export",
    )?;
    Ok(output.stdout)
}

fn persist_swift_lcov(runtime: &Runtime, contents: &[u8]) -> Result<PathBuf> {
    let lcov = runtime.temporary.path().join("swift.lcov");
    fs::write(&lcov, contents)?;
    Ok(lcov)
}

fn swift_metrics(config: &GateConfig, runtime: &Runtime) -> Result<Vec<SwiftMetric>> {
    selected_swift_sources(config, &runtime.repo_root).and_then(|sources| {
        build_swift_analyzer(config, runtime).and_then(|()| {
            swift_analyzer_path(config, runtime)
                .and_then(|analyzer| run_swift_analyzer(&analyzer, sources))
        })
    })
}

fn selected_swift_sources(config: &GateConfig, repo_root: &Path) -> Result<Vec<PathBuf>> {
    compile_excludes(&config.exclude).and_then(|excludes| {
        swift_source_files(&config.swift_root, repo_root, &excludes).and_then(ensure_swift_sources)
    })
}

fn ensure_swift_sources(sources: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    if sources.is_empty() {
        bail!("no Swift production functions were selected");
    }
    Ok(sources)
}

fn build_swift_analyzer(config: &GateConfig, runtime: &Runtime) -> Result<()> {
    let mut command = swift_analyzer_build_command(config, false);
    swift_environment(&mut command, &runtime.temporary);
    checked_status(&mut command, "build SwiftSyntax analyzer")
}

fn swift_analyzer_path(config: &GateConfig, runtime: &Runtime) -> Result<PathBuf> {
    let mut command = swift_analyzer_build_command(config, true);
    swift_environment(&mut command, &runtime.temporary);
    checked_output(&mut command, "locate SwiftSyntax analyzer")
        .and_then(output_path)
        .map(|path| path.join("swift-crap-analyzer"))
}

fn swift_analyzer_build_command(config: &GateConfig, show_bin_path: bool) -> Command {
    let mut command = Command::new(&config.swift);
    command
        .args(["build", "--package-path"])
        .arg(&config.swift_analyzer_package)
        .args(["--configuration", "release"]);
    if show_bin_path {
        command.arg("--show-bin-path");
    }
    command
}

fn run_swift_analyzer(analyzer: &Path, sources: Vec<PathBuf>) -> Result<Vec<SwiftMetric>> {
    checked_output(Command::new(analyzer).args(sources), "SwiftSyntax analyzer").and_then(
        |output| serde_json::from_slice(&output.stdout).context("parse Swift analyzer JSON"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(repo_root: &Path) -> GateConfig {
        GateConfig {
            top: 10,
            repo_root: repo_root.to_path_buf(),
            rust_manifest: repo_root.join("Cargo.toml"),
            rust_root: repo_root.to_path_buf(),
            swift_package: None,
            swift_root: Vec::new(),
            exclude: vec!["generated/**".into()],
            rust_coverage_ignore_regex: Some("generated".into()),
            report_json: None,
            cargo_crap: "cargo-crap".into(),
            cargo_llvm_cov: "cargo-llvm-cov".into(),
            swift: "swift".into(),
            swift_analyzer_package: repo_root.join("tools/analyzer"),
            llvm_cov: Some(PathBuf::from("llvm-cov")),
            llvm_profdata: Some(PathBuf::from("llvm-profdata")),
        }
    }

    #[test]
    fn invalid_config_is_analysis_failure() {
        let root = tempfile::tempdir().unwrap();
        let mut config = config(root.path());
        config.top = 0;
        assert_eq!(run(config), EXIT_ANALYSIS_FAILED);
    }

    #[test]
    fn runtime_accepts_explicit_llvm_tools() {
        let root = tempfile::tempdir().unwrap();
        let config = config(root.path());
        let runtime = Runtime::new(&config).unwrap();
        assert_eq!(runtime.repo_root, root.path().canonicalize().unwrap());
        assert_eq!(runtime.llvm_cov, PathBuf::from("llvm-cov"));
        assert_eq!(runtime.llvm_profdata, PathBuf::from("llvm-profdata"));
    }

    #[test]
    fn swift_pair_is_validated_together() {
        let root = tempfile::tempdir().unwrap();
        let mut config = config(root.path());
        config.swift_package = Some(root.path().join("Package.swift"));
        assert!(validate_config(&config).is_err());
    }

    #[test]
    fn package_test_binary_requires_one_real_bundle_binary() {
        let root = tempfile::tempdir().unwrap();
        let binary = root
            .path()
            .join("Fixture.xctest/Contents/MacOS/FixturePackageTests");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::write(&binary, "").unwrap();
        assert_eq!(package_test_binary(root.path()).unwrap(), binary);
        let symbol = root
            .path()
            .join("Fixture.dSYM/Fixture.xctest/Contents/MacOS/OtherPackageTests");
        fs::create_dir_all(symbol.parent().unwrap()).unwrap();
        fs::write(symbol, "").unwrap();
        assert_eq!(package_test_binary(root.path()).unwrap(), binary);
    }

    #[test]
    fn command_builders_pin_policy_inputs() {
        let root = tempfile::tempdir().unwrap();
        let config = config(root.path());
        let runtime = Runtime::new(&config).unwrap();
        let exports = parse_shell_exports("export A='one'\nexport B='two'\n").unwrap();
        assert_eq!(exports, [("A".into(), "one".into()), ("B".into(), "two".into())]);
        let coverage = rust_coverage_export_command(
            &config,
            &runtime,
            Path::new("coverage.profdata"),
            &[PathBuf::from("covered-binary")],
        );
        let coverage_debug = format!("{coverage:?}");
        assert!(coverage_debug.contains("--ignore-filename-regex"));
        assert!(coverage_debug.contains("generated"));
        let report = cargo_crap_command(
            &config,
            Path::new("coverage.lcov"),
            Path::new("report.json"),
        );
        let report_debug = format!("{report:?}");
        assert!(report_debug.contains("--missing"));
        assert!(report_debug.contains("pessimistic"));
        assert!(report_debug.contains("--exclude"));
    }

    #[test]
    fn json_report_is_optional_and_exhaustive() {
        let root = tempfile::tempdir().unwrap();
        write_json_report(None, &[]).unwrap();
        let path = root.path().join("report.json");
        let entry = crate::Entry {
            language: "rust".into(),
            file: "src/lib.rs".into(),
            function: "f".into(),
            line: 1,
            complexity: 1.0,
            coverage: 100.0,
            coverage_missing: false,
            crap: 1.0,
        };
        write_json_report(Some(&path), &[entry]).unwrap();
        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.contains("\"threshold\": 15.0"));
        assert!(contents.contains("\"function\": \"f\""));
    }
}
