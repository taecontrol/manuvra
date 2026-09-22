use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path};

const PROFILE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "lowercase")]
enum Platform {
    Linux,
    Darwin,
}

impl Platform {
    fn name(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Darwin => "darwin",
        }
    }

    fn target_os(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Darwin => "macos",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileDocument {
    schema_version: u32,
    profiles: Vec<Profile>,
    platform_sources: Vec<PlatformSource>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    platform: Platform,
    target_os: String,
    exclude: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlatformSource {
    path: String,
    platform: Platform,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct Selection {
    pub name: &'static str,
    pub exclude: Vec<String>,
}

pub(super) fn resolve(
    document_path: &Path,
    rust_root: &Path,
    target_os: &str,
) -> Result<Selection> {
    let document: ProfileDocument = serde_json::from_slice(
        &fs::read(document_path)
            .with_context(|| format!("read platform profiles {}", document_path.display()))?,
    )
    .context("parse platform profiles")?;
    validate_and_select(document, rust_root, target_os)
}

fn validate_and_select(
    document: ProfileDocument,
    rust_root: &Path,
    target_os: &str,
) -> Result<Selection> {
    if document.schema_version != PROFILE_SCHEMA_VERSION {
        bail!(
            "platform profile schema must be {PROFILE_SCHEMA_VERSION}; got {}",
            document.schema_version
        );
    }
    let sources = validated_sources(document.platform_sources, rust_root)?;
    let profiles = validated_profiles(document.profiles, &sources)?;
    let (platform, profile) = profiles
        .into_iter()
        .find(|(_, profile)| profile.target_os == target_os)
        .with_context(|| format!("unsupported compiled platform target_os {target_os}"))?;
    Ok(Selection {
        name: platform.name(),
        exclude: profile.exclude,
    })
}

fn validated_sources(
    platform_sources: Vec<PlatformSource>,
    rust_root: &Path,
) -> Result<BTreeMap<String, Platform>> {
    let mut sources = BTreeMap::new();
    for source in platform_sources {
        validate_source_path(&source.path, rust_root)?;
        if sources
            .insert(source.path.clone(), source.platform)
            .is_some()
        {
            bail!("platform source is repeated: {}", source.path);
        }
    }
    Ok(sources)
}

fn validate_source_path(source: &str, rust_root: &Path) -> Result<()> {
    let relative = Path::new(source);
    if relative.extension().and_then(|value| value.to_str()) != Some("rs")
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("platform source must be a relative Rust source path: {source}");
    }
    let root = rust_root
        .canonicalize()
        .with_context(|| format!("canonicalize Rust root {}", rust_root.display()))?;
    let resolved = root.join(relative);
    let canonical = resolved
        .canonicalize()
        .with_context(|| format!("resolve platform source {}", resolved.display()))?;
    if !canonical.starts_with(&root) || !canonical.is_file() {
        bail!("platform source is outside the Rust root: {source}");
    }
    Ok(())
}

fn validated_profiles(
    profiles: Vec<Profile>,
    sources: &BTreeMap<String, Platform>,
) -> Result<BTreeMap<Platform, Profile>> {
    let mut validated = BTreeMap::new();
    for profile in profiles {
        if profile.target_os != profile.platform.target_os() {
            bail!(
                "profile {:?} must declare target_os {}",
                profile.platform,
                profile.platform.target_os()
            );
        }
        validate_exclusions(&profile, sources)?;
        let platform = profile.platform;
        if validated.insert(platform, profile).is_some() {
            bail!("profile is repeated for {platform:?}");
        }
    }
    for platform in [Platform::Linux, Platform::Darwin] {
        if !validated.contains_key(&platform) {
            bail!("missing profile for {platform:?}");
        }
    }
    Ok(validated)
}

fn validate_exclusions(profile: &Profile, sources: &BTreeMap<String, Platform>) -> Result<()> {
    let actual = profile.exclude.iter().cloned().collect::<BTreeSet<_>>();
    if actual.len() != profile.exclude.len() {
        bail!("profile {:?} repeats an exclusion", profile.platform);
    }
    for excluded in &actual {
        let owner = sources.get(excluded).with_context(|| {
            format!(
                "profile {:?} excludes shared or unclassified source {excluded}",
                profile.platform
            )
        })?;
        if *owner == profile.platform {
            bail!(
                "profile {:?} excludes current-platform source {excluded}",
                profile.platform
            );
        }
    }
    let expected = sources
        .iter()
        .filter(|(_, owner)| **owner != profile.platform)
        .map(|(path, _)| path.clone())
        .collect::<BTreeSet<_>>();
    if actual != expected {
        bail!(
            "profile {:?} exclusions do not match the other platform's sources",
            profile.platform
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        for source in ["shared.rs", "linux.rs", "darwin.rs"] {
            fs::write(root.path().join(source), "fn covered() {}\n").unwrap();
        }
        let document = root.path().join("profiles.json");
        (root, document)
    }

    fn write_document(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn selects_exact_profiles_for_linux_and_darwin() {
        let (root, document) = fixture();
        write_document(
            &document,
            r#"{
              "schema_version": 1,
              "profiles": [
                {"platform":"linux","target_os":"linux","exclude":["darwin.rs"]},
                {"platform":"darwin","target_os":"macos","exclude":["linux.rs"]}
              ],
              "platform_sources": [
                {"path":"linux.rs","platform":"linux"},
                {"path":"darwin.rs","platform":"darwin"}
              ]
            }"#,
        );
        assert_eq!(
            resolve(&document, root.path(), "linux").unwrap(),
            Selection {
                name: "linux",
                exclude: vec!["darwin.rs".into()]
            }
        );
        assert_eq!(
            resolve(&document, root.path(), "macos").unwrap(),
            Selection {
                name: "darwin",
                exclude: vec!["linux.rs".into()]
            }
        );
    }

    #[test]
    fn rejects_shared_and_current_platform_exclusions() {
        let (root, document) = fixture();
        for excluded in ["shared.rs", "linux.rs"] {
            write_document(
                &document,
                &format!(
                    r#"{{
                      "schema_version": 1,
                      "profiles": [
                        {{"platform":"linux","target_os":"linux","exclude":["{excluded}"]}},
                        {{"platform":"darwin","target_os":"macos","exclude":["linux.rs"]}}
                      ],
                      "platform_sources": [{{"path":"linux.rs","platform":"linux"}}]
                    }}"#
                ),
            );
            assert!(resolve(&document, root.path(), "linux").is_err());
        }
    }

    #[test]
    fn rejects_unknown_mismatched_and_incomplete_platforms() {
        let (root, document) = fixture();
        for contents in [
            r#"{"schema_version":1,"profiles":[{"platform":"windows","target_os":"windows","exclude":[]}],"platform_sources":[]}"#,
            r#"{"schema_version":1,"profiles":[{"platform":"linux","target_os":"macos","exclude":[]},{"platform":"darwin","target_os":"macos","exclude":[]}],"platform_sources":[]}"#,
            r#"{"schema_version":1,"profiles":[{"platform":"linux","target_os":"linux","exclude":[]},{"platform":"darwin","target_os":"macos","exclude":[]}],"platform_sources":[{"path":"linux.rs","platform":"linux"}]}"#,
        ] {
            write_document(&document, contents);
            assert!(resolve(&document, root.path(), "linux").is_err());
        }
        write_document(
            &document,
            r#"{"schema_version":1,"profiles":[{"platform":"linux","target_os":"linux","exclude":[]},{"platform":"darwin","target_os":"macos","exclude":[]}],"platform_sources":[]}"#,
        );
        assert!(resolve(&document, root.path(), "freebsd").is_err());
    }

    #[test]
    fn repository_profiles_classify_only_platform_siblings() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let document = manifest_dir.join("platform-profiles.json");
        let rust_root = manifest_dir.join("../../crates");
        assert_eq!(
            resolve(&document, &rust_root, "linux").unwrap().exclude,
            [
                "manuvra-cli/src/client/darwin.rs",
                "manuvra-cli/src/process/darwin.rs",
                "manuvra-cli/src/runtime/darwin.rs",
                "manuvra-cli/src/socket_auth/darwin.rs"
            ]
        );
        assert_eq!(
            resolve(&document, &rust_root, "macos").unwrap().exclude,
            [
                "manuvra-cli/src/host.rs",
                "manuvra-cli/src/process/linux.rs",
                "manuvra-cli/src/runtime/linux.rs",
                "manuvra-cli/src/socket_auth/linux.rs"
            ]
        );
    }
}
