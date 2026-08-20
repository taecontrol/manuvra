use serde_json::Value;
use std::fs;

pub(crate) fn write(name: &str, value: &Value) {
    let Some(root) = std::env::var_os("MANUVRA_CP07_BOUNDARY_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join(name), serde_json::to_vec_pretty(value).unwrap()).unwrap();
}
