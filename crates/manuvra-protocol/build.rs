use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let accepted_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets");
    let registry_path = accepted_root.join("registry.json");
    let catalog_path = accepted_root.join("error-catalog.json");
    println!("cargo:rerun-if-changed={}", registry_path.display());
    println!("cargo:rerun-if-changed={}", catalog_path.display());
    let registry: Value = serde_json::from_slice(&fs::read(registry_path).expect("registry bytes"))
        .expect("accepted registry JSON");
    let catalog: Value = serde_json::from_slice(&fs::read(catalog_path).expect("catalog bytes"))
        .expect("accepted error catalog JSON");
    let commands = registry["commands"].as_array().expect("registry commands");
    let command_variants = commands.iter().map(command_pair).collect::<Vec<_>>();
    let error_variants = catalog_pairs(&catalog, "errors");
    let warning_variants = catalog_pairs(&catalog, "warnings");
    let source = generated_source(&command_variants, &error_variants, &warning_variants);
    let output = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    fs::write(output.join("command_ids.rs"), source).expect("generated command IDs");
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf();
    let workspace_digest = workspace_build_digest(&workspace);
    let release_manifest = release_manifest(&workspace, &workspace_digest);
    let build_id = release_manifest["build_id"]
        .as_str()
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .expect("release manifest build_id must be a SHA-256 hex string");
    println!("cargo:rustc-env=MANUVRA_BUILD_DIGEST={build_id}");
    fs::write(
        output.join("release_manifest.rs"),
        format!(
            "pub const RELEASE_MANIFEST_JSON: &str = {:?};\n",
            serde_json::to_string(&release_manifest).expect("release manifest JSON")
        ),
    )
    .expect("generated release manifest");
}

fn release_manifest(workspace: &Path, workspace_digest: &str) -> Value {
    println!("cargo:rerun-if-env-changed=MANUVRA_RELEASE_MANIFEST_PATH");
    if let Some(path) = std::env::var_os("MANUVRA_RELEASE_MANIFEST_PATH") {
        let path = PathBuf::from(path);
        println!("cargo:rerun-if-changed={}", path.display());
        return serde_json::from_slice(&fs::read(path).expect("release manifest bytes"))
            .expect("release manifest JSON");
    }
    serde_json::json!({
        "schema": "manuvra/release-manifest@1",
        "version": std::env::var("CARGO_PKG_VERSION").expect("package version"),
        "source_tree_sha256": workspace_digest,
        "build_id": workspace_digest,
        "cargo_lock_sha256": file_digest(&workspace.join("Cargo.lock")),
        "supported_target": std::env::var("TARGET").unwrap_or_else(|_| "development".to_owned()),
        "archive_format": 1,
        "resources": {},
    })
}

fn file_digest(path: &Path) -> String {
    Sha256::digest(fs::read(path).expect("digest input"))
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn workspace_build_digest(workspace: &Path) -> String {
    let mut files = vec![workspace.join("Cargo.toml"), workspace.join("Cargo.lock")];
    collect_product_files(&workspace.join("crates"), &mut files);
    files.sort();
    let mut digest = Sha256::new();
    for key in ["TARGET", "HOST", "PROFILE", "OPT_LEVEL", "DEBUG"] {
        let value = std::env::var(key).unwrap_or_default();
        digest.update((key.len() as u64).to_be_bytes());
        digest.update(key.as_bytes());
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
    }
    let mut features = std::env::vars()
        .filter(|(key, value)| key.starts_with("CARGO_FEATURE_") && value == "1")
        .map(|(key, _)| key)
        .collect::<Vec<_>>();
    features.sort();
    for feature in features {
        digest.update((feature.len() as u64).to_be_bytes());
        digest.update(feature.as_bytes());
    }
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    let rustc_version = Command::new(&rustc)
        .arg("--version")
        .arg("--verbose")
        .output()
        .expect("query rustc build identity");
    assert!(
        rustc_version.status.success(),
        "rustc identity command failed"
    );
    digest.update((rustc_version.stdout.len() as u64).to_be_bytes());
    digest.update(&rustc_version.stdout);
    for path in files {
        println!("cargo:rerun-if-changed={}", path.display());
        let relative = path.strip_prefix(workspace).expect("workspace file");
        let bytes = fs::read(&path).expect("product source bytes");
        let name = relative.to_string_lossy();
        digest.update((name.len() as u64).to_be_bytes());
        digest.update(name.as_bytes());
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn collect_product_files(root: &Path, files: &mut Vec<PathBuf>) {
    let mut entries = fs::read_dir(root)
        .expect("product source directory")
        .map(|entry| entry.expect("product source entry").path())
        .collect::<Vec<_>>();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_product_files(&path, files);
        } else if is_product_source(&path) {
            files.push(path);
        }
    }
}

fn is_product_source(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("rs" | "toml" | "json" | "md")
    )
}

fn catalog_pairs(catalog: &Value, key: &str) -> Vec<(String, String)> {
    catalog[key]
        .as_array()
        .expect("catalog entries")
        .iter()
        .map(|entry| {
            let code = entry["code"].as_str().expect("catalog code").to_owned();
            (variant_name(&code), code)
        })
        .collect()
}

fn command_pair(command: &Value) -> (String, String) {
    let id = command["id"].as_str().expect("command ID").to_owned();
    (variant_name(&id), id)
}

fn variant_name(id: &str) -> String {
    id.split(['.', '_'])
        .map(capitalize)
        .collect::<Vec<_>>()
        .join("")
}

fn capitalize(part: &str) -> String {
    let mut characters = part.chars();
    let first = characters.next().expect("non-empty command part");
    format!("{}{}", first.to_ascii_uppercase(), characters.as_str())
}

fn generated_source(
    commands: &[(String, String)],
    errors: &[(String, String)],
    warnings: &[(String, String)],
) -> String {
    [
        enum_source("CommandId", commands),
        enum_source("ErrorCode", errors),
        enum_source("WarningCode", warnings),
    ]
    .join("\n")
}

fn enum_source(name: &str, variants: &[(String, String)]) -> String {
    let declarations = variants
        .iter()
        .map(|(variant, _)| format!("    {variant},"))
        .collect::<Vec<_>>()
        .join("\n");
    let parsing = variants
        .iter()
        .map(|(variant, id)| format!("            \"{id}\" => Some(Self::{variant}),"))
        .collect::<Vec<_>>()
        .join("\n");
    let strings = variants
        .iter()
        .map(|(variant, id)| format!("            Self::{variant} => \"{id}\","))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]\n\
         pub enum {name} {{\n{declarations}\n}}\n\
         impl {name} {{\n\
             pub fn parse(value: &str) -> Option<Self> {{\n\
                 match value {{\n{parsing}\n                    _ => None,\n                }}\n            }}\n\
             pub fn as_str(self) -> &'static str {{\n\
                 match self {{\n{strings}\n                }}\n            }}\n\
         }}\n"
    )
}
