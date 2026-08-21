use manuvra_protocol::{build_digest, embedded_resource, release_manifest, sha256_hex};
use serde_json::{Map, Value, json};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

pub const BUNDLE_IDENTIFIER: &str = "com.taecontrol.manuvra";

#[derive(Debug, Clone)]
pub struct Installation {
    pub executable: PathBuf,
    pub daemon: PathBuf,
    pub bundle: Option<PathBuf>,
    pub resources: PathBuf,
    pub installed: bool,
}

impl Installation {
    pub fn current() -> Result<Self, InstallationError> {
        let executable = fs::canonicalize(std::env::current_exe()?)?;
        if let Some(installed) = Self::from_installed_executable(executable.clone())? {
            installed.verify()?;
            return Ok(installed);
        }
        if cfg!(debug_assertions) {
            return Self::development(executable);
        }
        Err(InstallationError::InvalidLayout(
            "release executable must be inside Manuvra.app/Contents/MacOS".to_owned(),
        ))
    }

    fn from_installed_executable(executable: PathBuf) -> Result<Option<Self>, InstallationError> {
        let Some((macos, contents, bundle)) = bundle_ancestors(&executable) else {
            return Ok(None);
        };
        if !supported_binary(&executable) || !installed_layout(macos, contents, bundle) {
            return Ok(None);
        }
        let daemon = fs::canonicalize(macos.join("manuvra-daemon"))?;
        let resources = fs::canonicalize(contents.join("Resources"))?;
        let bundle = bundle.to_path_buf();
        Ok(Some(Self {
            executable,
            daemon,
            bundle: Some(bundle),
            resources,
            installed: true,
        }))
    }

    fn development(executable: PathBuf) -> Result<Self, InstallationError> {
        let daemon = if let Some(path) = std::env::var_os("MANUVRA_DAEMON_PATH") {
            fs::canonicalize(path)?
        } else {
            executable.with_file_name("manuvra-daemon")
        };
        let resources = if let Some(path) = std::env::var_os("MANUVRA_RESOURCE_ROOT") {
            fs::canonicalize(path)?
        } else {
            fs::canonicalize(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../manuvra-protocol/assets"),
            )?
        };
        Ok(Self {
            executable,
            daemon,
            bundle: None,
            resources,
            installed: false,
        })
    }

    pub fn verify(&self) -> Result<(), InstallationError> {
        verify_daemon(&self.daemon)?;
        if !self.installed {
            return Ok(());
        }
        self.verify_installed()
    }

    fn verify_installed(&self) -> Result<(), InstallationError> {
        self.verify_plist()?;
        let manifest = release_manifest();
        verify_manifest_file(&self.resources, &manifest)?;
        verify_manifest_identity(&manifest)?;
        verify_manifest_resources(&self.resources, &manifest)
    }

    fn verify_plist(&self) -> Result<(), InstallationError> {
        let bundle = self.bundle.as_ref().ok_or_else(|| {
            InstallationError::InvalidLayout("installed bundle is missing".to_owned())
        })?;
        let plist = fs::read_to_string(bundle.join("Contents/Info.plist"))?;
        let required = [
            BUNDLE_IDENTIFIER,
            "manuvra-daemon",
            "<string>APPL</string>",
            "<string>26.0</string>",
        ];
        if let Some(missing) = required
            .into_iter()
            .find(|required| !plist.contains(required))
        {
            return Err(InstallationError::InvalidLayout(format!(
                "Info.plist is missing {missing}"
            )));
        }
        Ok(())
    }

    pub fn identity(&self) -> Value {
        let signature = self.signature_identity();
        json!({
            "installed": self.installed,
            "executable": self.executable,
            "daemon": self.daemon,
            "bundle": self.bundle,
            "bundle_id": BUNDLE_IDENTIFIER,
            "build_id": build_digest(),
            "resource_manifest_sha256": sha256_hex(manuvra_protocol::RELEASE_MANIFEST_JSON.as_bytes()),
            "cdhash": signature.cdhash,
            "authority": signature.authority,
            "designated_requirement": signature.designated_requirement,
        })
    }

    fn signature_identity(&self) -> SignatureIdentity {
        self.bundle
            .as_deref()
            .map(inspect_signature)
            .unwrap_or_default()
    }

    pub fn examples(&self) -> Value {
        let commands = manuvra_protocol::registry()["commands"]
            .as_array()
            .expect("registry commands")
            .iter()
            .map(|command| {
                (
                    command["id"].as_str().expect("command ID").to_owned(),
                    command["examples"].clone(),
                )
            })
            .collect::<Map<_, _>>();
        Value::Object(commands)
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SignatureIdentity {
    cdhash: Option<String>,
    authority: Option<String>,
    designated_requirement: Option<String>,
}

fn inspect_signature(path: &Path) -> SignatureIdentity {
    parse_signature_identity(
        &codesign_stderr(&["-dv", "--verbose=4"], path).unwrap_or_default(),
        &codesign_designated_text(path).unwrap_or_default(),
    )
}

fn codesign_output(args: &[&str], path: &Path) -> Option<std::process::Output> {
    let output = Command::new("/usr/bin/codesign")
        .args(args)
        .arg(path)
        .output()
        .ok()?;
    output.status.success().then_some(output)
}

fn codesign_stderr(args: &[&str], path: &Path) -> Option<String> {
    codesign_output(args, path).map(|output| String::from_utf8_lossy(&output.stderr).into_owned())
}

fn codesign_designated_text(path: &Path) -> Option<String> {
    let output = codesign_output(&["-d", "-r-"], path)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.contains("designated =>") {
        Some(stdout.into_owned())
    } else {
        Some(format!(
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn parse_signature_identity(display: &str, designated: &str) -> SignatureIdentity {
    SignatureIdentity {
        cdhash: first_prefixed_value(display, "CDHash="),
        authority: first_prefixed_value(display, "Authority="),
        designated_requirement: designated_requirement(designated),
    }
}

fn first_prefixed_value(text: &str, prefix: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn designated_requirement(text: &str) -> Option<String> {
    const MARKER: &str = "designated => ";
    text.lines()
        .find_map(|line| {
            line.find(MARKER)
                .map(|index| line[index + MARKER.len()..].trim())
        })
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn bundle_ancestors(executable: &Path) -> Option<(&Path, &Path, &Path)> {
    let macos = executable.parent()?;
    let contents = macos.parent()?;
    Some((macos, contents, contents.parent()?))
}

fn supported_binary(executable: &Path) -> bool {
    matches!(
        executable.file_name().and_then(|value| value.to_str()),
        Some("manuvra" | "manuvra-daemon")
    )
}

fn installed_layout(macos: &Path, contents: &Path, bundle: &Path) -> bool {
    [
        (macos, "MacOS"),
        (contents, "Contents"),
        (bundle, "Manuvra.app"),
        (bundle.parent().unwrap_or(Path::new("")), "libexec"),
    ]
    .into_iter()
    .all(|(path, expected)| path.file_name().and_then(|value| value.to_str()) == Some(expected))
}

fn verify_daemon(daemon: &Path) -> Result<(), InstallationError> {
    daemon.is_file().then_some(()).ok_or_else(|| {
        InstallationError::InvalidLayout("exact daemon sibling is missing".to_owned())
    })
}

fn verify_manifest_file(resources: &Path, manifest: &Value) -> Result<(), InstallationError> {
    let installed: Value =
        serde_json::from_slice(&fs::read(resources.join("release-manifest.json"))?)?;
    (installed == *manifest).then_some(()).ok_or_else(|| {
        InstallationError::ResourceMismatch(
            "release-manifest.json differs from the embedded manifest".to_owned(),
        )
    })
}

fn verify_manifest_identity(manifest: &Value) -> Result<(), InstallationError> {
    (manifest["build_id"].as_str() == Some(&build_digest()))
        .then_some(())
        .ok_or_else(|| {
            InstallationError::ResourceMismatch(
                "embedded build ID differs from the release manifest".to_owned(),
            )
        })
}

fn verify_embedded_resource(root: &Path, relative: &str) -> Result<(), InstallationError> {
    let Some(embedded) = embedded_resource(relative) else {
        return Ok(());
    };
    (fs::read(root.join(relative))? == embedded)
        .then_some(())
        .ok_or_else(|| {
            InstallationError::ResourceMismatch(format!(
                "installed {relative} differs from its embedded bytes"
            ))
        })
}

fn verify_manifest_resources(root: &Path, manifest: &Value) -> Result<(), InstallationError> {
    manifest["resources"]
        .as_object()
        .ok_or_else(|| {
            InstallationError::ResourceMismatch("manifest resources must be an object".to_owned())
        })?
        .iter()
        .try_for_each(|(relative, expected)| {
            verify_relative_resource(root, relative, expected)?;
            verify_embedded_resource(root, relative)
        })
}

fn verify_relative_resource(
    root: &Path,
    relative: &str,
    expected: &Value,
) -> Result<(), InstallationError> {
    verify_safe_relative_path(relative)?;
    let expected_digest = expected.as_str().ok_or_else(|| {
        InstallationError::ResourceMismatch(format!("invalid digest for {relative}"))
    })?;
    let path = fs::canonicalize(root.join(relative))?;
    verify_contained_resource(root, relative, &path)?;
    verify_resource_digest(relative, expected_digest, &path)
}

fn verify_safe_relative_path(relative: &str) -> Result<(), InstallationError> {
    Path::new(relative)
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
        .then_some(())
        .ok_or_else(|| {
            InstallationError::ResourceMismatch(format!("unsafe resource path {relative}"))
        })
}

fn verify_contained_resource(
    root: &Path,
    relative: &str,
    path: &Path,
) -> Result<(), InstallationError> {
    path.starts_with(root).then_some(()).ok_or_else(|| {
        InstallationError::ResourceMismatch(format!("resource escapes the bundle: {relative}"))
    })
}

fn verify_resource_digest(
    relative: &str,
    expected: &str,
    path: &Path,
) -> Result<(), InstallationError> {
    (sha256_hex(&fs::read(path)?) == expected)
        .then_some(())
        .ok_or_else(|| {
            InstallationError::ResourceMismatch(format!("digest mismatch for {relative}"))
        })
}

#[derive(Debug, thiserror::Error)]
pub enum InstallationError {
    #[error("installation I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("installation JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid installed layout: {0}")]
    InvalidLayout(String),
    #[error("installed resource mismatch: {0}")]
    ResourceMismatch(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_layout_and_resource_validation_fail_closed() {
        let temporary = tempfile::tempdir().unwrap();
        let prefix = temporary.path();
        let bundle = prefix.join("libexec/Manuvra.app");
        let macos = bundle.join("Contents/MacOS");
        let resources = bundle.join("Contents/Resources");
        fs::create_dir_all(&macos).unwrap();
        fs::create_dir_all(&resources).unwrap();
        let executable = macos.join("manuvra");
        fs::write(&executable, b"cli").unwrap();
        fs::write(macos.join("manuvra-daemon"), b"daemon").unwrap();
        fs::write(
            bundle.join("Contents/Info.plist"),
            format!(
                "{BUNDLE_IDENTIFIER} manuvra-daemon <string>APPL</string> <string>26.0</string>"
            ),
        )
        .unwrap();
        fs::write(
            resources.join("release-manifest.json"),
            manuvra_protocol::RELEASE_MANIFEST_JSON,
        )
        .unwrap();

        let installation = Installation::from_installed_executable(executable.clone())
            .unwrap()
            .unwrap();
        installation.verify().unwrap();
        assert!(
            Installation::from_installed_executable(macos.join("other"))
                .unwrap()
                .is_none()
        );

        let resources = installation.resources.clone();
        let sample = resources.join("sample");
        fs::write(&sample, b"sample").unwrap();
        verify_relative_resource(&resources, "sample", &Value::String(sha256_hex(b"sample")))
            .unwrap();
        assert!(verify_relative_resource(&resources, "../sample", &Value::Null).is_err());
        assert!(
            verify_relative_resource(&resources, "sample", &Value::String("bad".to_owned()))
                .is_err()
        );

        fs::write(bundle.join("Contents/Info.plist"), "incomplete").unwrap();
        assert!(installation.verify().is_err());
    }

    #[test]
    fn identity_json_always_includes_grant_stable_codesign_fields() {
        let installation = Installation {
            executable: PathBuf::from("/tmp/manuvra"),
            daemon: PathBuf::from("/tmp/manuvra-daemon"),
            bundle: None,
            resources: PathBuf::from("/tmp/Resources"),
            installed: false,
        };
        let identity = installation.identity();
        let object = identity.as_object().expect("identity object");
        for key in ["cdhash", "authority", "designated_requirement"] {
            assert!(
                object.contains_key(key),
                "identity must report {key} so a new CDHash is not mistaken for a new grant identity"
            );
            assert!(
                object[key].is_null(),
                "development layouts have no bundle signature to inspect"
            );
        }
    }

    #[test]
    fn signature_parser_distinguishes_adhoc_cdhash_from_named_authority() {
        let adhoc = parse_signature_identity(
            "Executable=/tmp/Manuvra.app\nIdentifier=com.taecontrol.manuvra\nFormat=app bundle with Mach-O thin (arm64)\nSignature=adhoc\nCDHash=0123456789abcdef0123456789abcdef01234567\n",
            "Executable=/tmp/Manuvra.app\ndesignated => cdhash H\"0123456789ABCDEF0123456789ABCDEF01234567\"\n",
        );
        assert_eq!(
            adhoc.cdhash.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        assert_eq!(adhoc.authority, None);
        assert_eq!(
            adhoc.designated_requirement.as_deref(),
            Some("cdhash H\"0123456789ABCDEF0123456789ABCDEF01234567\"")
        );

        let named = parse_signature_identity(
            "Executable=/tmp/Manuvra.app\nIdentifier=com.taecontrol.manuvra\nAuthority=Manuvra Local\nAuthority=Manuvra Local CA\nCDHash=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
            "designated => identifier \"com.taecontrol.manuvra\" and certificate leaf = H\"BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"\n",
        );
        assert_eq!(
            named.cdhash.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(named.authority.as_deref(), Some("Manuvra Local"));
        assert_eq!(
            named.designated_requirement.as_deref(),
            Some(
                "identifier \"com.taecontrol.manuvra\" and certificate leaf = H\"BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\""
            )
        );
    }

    #[test]
    fn failed_codesign_inspect_does_not_invent_a_signature_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("not-mach-o");
        fs::write(&path, b"not a signed Mach-O").unwrap();
        assert_eq!(inspect_signature(&path), SignatureIdentity::default());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn live_adhoc_signature_is_cdhash_requirement_without_authority() {
        let temporary = tempfile::tempdir().unwrap();
        let binary = temporary.path().join("tool");
        fs::copy("/usr/bin/true", &binary).unwrap();
        assert!(
            Command::new("/usr/bin/codesign")
                .args(["--force", "--sign", "-"])
                .arg(&binary)
                .status()
                .unwrap()
                .success()
        );
        let parsed = inspect_signature(&binary);
        assert!(parsed.cdhash.is_some());
        assert_eq!(parsed.authority, None);
        assert!(
            parsed
                .designated_requirement
                .as_deref()
                .is_some_and(|value| value.starts_with("cdhash ")),
            "ad-hoc designated requirement was {:?}",
            parsed.designated_requirement
        );

        let identity = Installation {
            executable: PathBuf::from("/tmp/manuvra"),
            daemon: PathBuf::from("/tmp/manuvra-daemon"),
            bundle: Some(binary),
            resources: PathBuf::from("/tmp/Resources"),
            installed: true,
        }
        .identity();
        assert!(identity["cdhash"].as_str().is_some());
        assert!(identity["authority"].is_null());
        assert!(
            identity["designated_requirement"]
                .as_str()
                .is_some_and(|value| value.starts_with("cdhash "))
        );
    }
}
