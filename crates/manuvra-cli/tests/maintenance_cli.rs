use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;

const CLI: &str = env!("CARGO_BIN_EXE_manuvra");

fn run(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(CLI)
        .args(args)
        .env("MANUVRA_CONFIG_HOME", root.join("config"))
        .env("MANUVRA_LEGACY_CONFIG_HOME", root.join("legacy"))
        .env("MANUVRA_TMPDIR", root.join("tmp"))
        .output()
        .unwrap()
}

#[test]
fn explicit_migration_preserves_source_and_purge_requires_confirmation() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("legacy")).unwrap();
    fs::write(root.path().join("legacy/usage.json"), b"{\"legacy\":true}").unwrap();

    let migrated = run(root.path(), &["migrate", "--from", "computer-use"]);
    assert!(migrated.status.success());
    let migrated: Value = serde_json::from_slice(&migrated.stdout).unwrap();
    assert_eq!(migrated["copied"], true);
    assert_eq!(migrated["source_preserved"], true);
    assert!(root.path().join("legacy/usage.json").is_file());
    assert!(root.path().join("config/usage.json").is_file());

    fs::create_dir_all(root.path().join("tmp/manuvra/runtime-v1")).unwrap();
    fs::write(root.path().join("tmp/manuvra/runtime-v1/state"), b"state").unwrap();
    let export = root.path().join("export/evidence.json");
    fs::create_dir_all(export.parent().unwrap()).unwrap();
    fs::write(&export, b"evidence").unwrap();

    let missing_all = run(root.path(), &["purge", "--yes"]);
    assert_eq!(missing_all.status.code(), Some(2));
    let missing_all: Value = serde_json::from_slice(&missing_all.stdout).unwrap();
    assert_eq!(missing_all["error"]["code"], "invalid_request");
    assert_eq!(missing_all["error"]["message"], "purge requires --all");
    assert!(root.path().join("config").exists());

    let unconfirmed = run(root.path(), &["purge", "--all"]);
    assert_eq!(unconfirmed.status.code(), Some(2));
    assert!(root.path().join("config").exists());

    let purged = run(root.path(), &["purge", "--all", "--yes"]);
    assert!(purged.status.success());
    let purged: Value = serde_json::from_slice(&purged.stdout).unwrap();
    assert_eq!(purged["retained_exports"], true);
    assert!(!root.path().join("config").exists());
    assert!(!root.path().join("tmp/manuvra").exists());
    assert_eq!(fs::read(export).unwrap(), b"evidence");
}
