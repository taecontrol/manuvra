use serde_json::Value;
use std::fs;
use std::path::PathBuf;

fn main() {
    let mut arguments = std::env::args_os().skip(1).map(PathBuf::from);
    let proof = arguments.next().expect("proof JSON path");
    let schema = arguments.next().expect("proof schema path");
    assert!(arguments.next().is_none(), "unexpected argument");
    let proof: Value =
        serde_json::from_slice(&fs::read(proof).expect("proof bytes")).expect("proof JSON");
    let schema: Value =
        serde_json::from_slice(&fs::read(schema).expect("schema bytes")).expect("schema JSON");
    if let Err(error) = manuvra_protocol::validate_external_document(&schema, &proof) {
        eprintln!("invalid public proof summary: {error}");
        std::process::exit(1);
    }
}
