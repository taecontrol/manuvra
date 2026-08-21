use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn print_codesign_identity(args: &[&str], env_identity: Option<&str>) -> std::process::Output {
    let mut command = Command::new(repo_root().join("scripts/package-manuvra.sh"));
    command.arg("--print-codesign-identity");
    command.args(args);
    command.env_remove("MANUVRA_CODESIGN_IDENTITY");
    if let Some(identity) = env_identity {
        command.env("MANUVRA_CODESIGN_IDENTITY", identity);
    }
    command.output().expect("package-manuvra.sh")
}

fn stdout_line(output: &std::process::Output) -> String {
    assert!(
        output.status.success(),
        "package-manuvra.sh failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

#[test]
fn default_package_identity_is_adhoc() {
    let output = print_codesign_identity(&[], None);
    assert_eq!(stdout_line(&output), "-");
}

#[test]
fn identity_flag_selects_named_codesign_identity() {
    let output = print_codesign_identity(&["--identity", "Manuvra Local"], None);
    assert_eq!(stdout_line(&output), "Manuvra Local");
}

#[test]
fn identity_env_selects_named_codesign_identity() {
    let output = print_codesign_identity(&[], Some("Manuvra Env"));
    assert_eq!(stdout_line(&output), "Manuvra Env");
}

#[test]
fn identity_flag_wins_over_env() {
    let output = print_codesign_identity(&["--identity", "Manuvra Flag"], Some("Manuvra Env"));
    assert_eq!(stdout_line(&output), "Manuvra Flag");
}

#[test]
fn empty_identity_flag_is_rejected() {
    let output = print_codesign_identity(&["--identity", ""], None);
    assert!(
        !output.status.success(),
        "empty --identity must not fall through to ad-hoc"
    );
}

#[test]
fn packager_signs_with_resolved_identity() {
    let script = fs::read_to_string(repo_root().join("scripts/package-manuvra.sh")).unwrap();
    assert!(
        script.contains("codesign --force --sign \"$identity\" --options runtime"),
        "packager must pass the resolved identity to codesign instead of hardcoding ad-hoc"
    );
    let matches = script
        .matches("codesign --force --sign \"$identity\" --options runtime")
        .count();
    assert_eq!(
        matches, 3,
        "cli, daemon, and bundle must share the resolved identity"
    );
    assert!(
        !script.contains("codesign --force --sign -"),
        "packager must not hardcode ad-hoc signing beside the resolved identity"
    );
}

#[test]
fn readme_distinguishes_packager_libexec_from_recommended_grant_path() {
    let readme = fs::read_to_string(repo_root().join("README.md")).unwrap();
    assert!(
        readme.contains(
            "`package-manuvra.sh --prefix DIR` always writes `DIR/libexec/Manuvra.app`. It does not write `~/Applications/Manuvra.app`."
        ),
        "README must not imply the packager installs ~/Applications/Manuvra.app"
    );
    assert!(
        readme.contains(
            "The recommended grant path is a single signed copy you keep at `~/Applications/Manuvra.app`"
        ),
        "README must still name ~/Applications/Manuvra.app as the recommended grant path"
    );
    let help = fs::read_to_string(repo_root().join("crates/manuvra-protocol/assets/agent-help.md"))
        .unwrap();
    assert!(
        help.contains(
            "`package-manuvra.sh --prefix DIR` writes `DIR/libexec/Manuvra.app`; it does not write `~/Applications/Manuvra.app`."
        ),
        "packaged help must not imply the packager installs ~/Applications/Manuvra.app"
    );
}

#[test]
fn packager_replaces_prefix_bin_symlink_on_rebuild() {
    let script = fs::read_to_string(repo_root().join("scripts/package-manuvra.sh")).unwrap();
    assert!(
        script.contains(
            r#"ln -sfn ../libexec/Manuvra.app/Contents/MacOS/manuvra "$prefix/bin/manuvra""#
        ),
        "rebuild to an existing prefix must replace bin/manuvra without following a dangling dest"
    );
    assert!(
        !script.contains(
            r#"ln -s ../libexec/Manuvra.app/Contents/MacOS/manuvra "$prefix/bin/manuvra""#
        ),
        "plain ln -s exits 1 when prefix/bin/manuvra already exists"
    );
}

#[test]
fn homebrew_formula_stays_adhoc() {
    let template = fs::read_to_string(repo_root().join("packaging/manuvra.rb.template")).unwrap();
    assert!(
        !template.contains("--identity"),
        "Homebrew bottles must not pass a named codesign identity"
    );
    assert!(
        template.contains(r#"ENV.delete("MANUVRA_CODESIGN_IDENTITY")"#),
        "Homebrew bottles must ignore a caller-exported named identity"
    );
    assert!(template.contains(r#"system "./scripts/package-manuvra.sh", "--prefix", prefix"#));
}
