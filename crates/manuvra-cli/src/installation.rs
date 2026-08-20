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
        json!({
            "installed": self.installed,
            "executable": self.executable,
            "daemon": self.daemon,
            "bundle": self.bundle,
            "bundle_id": BUNDLE_IDENTIFIER,
            "build_id": build_digest(),
            "resource_manifest_sha256": sha256_hex(manuvra_protocol::RELEASE_MANIFEST_JSON.as_bytes()),
            "cdhash": self.cdhash(),
        })
    }

    fn cdhash(&self) -> Option<String> {
        let bundle = self.bundle.as_ref()?;
        let output = Command::new("/usr/bin/codesign")
            .args(["-dv", "--verbose=4"])
            .arg(bundle)
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stderr);
        text.lines()
            .find_map(|line| line.strip_prefix("CDHash="))
            .map(str::to_owned)
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
}
