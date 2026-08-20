use agent_manuvra_crap_gate::{GateConfig, run};
use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(about = "Run the accepted function-level CRAP gate")]
struct Args {
    #[arg(long, default_value_t = 10)]
    top: usize,
    #[arg(long)]
    repo_root: PathBuf,
    #[arg(long)]
    rust_manifest: PathBuf,
    #[arg(long)]
    rust_root: PathBuf,
    #[arg(long)]
    swift_package: Option<PathBuf>,
    #[arg(long)]
    swift_root: Vec<PathBuf>,
    #[arg(long)]
    exclude: Vec<String>,
    #[arg(long)]
    rust_coverage_ignore_regex: Option<String>,
    #[arg(long)]
    report_json: Option<PathBuf>,
    #[arg(long, default_value = "cargo-crap")]
    cargo_crap: String,
    #[arg(long, default_value = "cargo-llvm-cov")]
    cargo_llvm_cov: String,
    #[arg(long, default_value = "swift")]
    swift: String,
    #[arg(long, default_value = "tools/swift-crap-analyzer")]
    swift_analyzer_package: PathBuf,
    #[arg(long)]
    llvm_cov: Option<PathBuf>,
    #[arg(long)]
    llvm_profdata: Option<PathBuf>,
}

impl From<Args> for GateConfig {
    fn from(args: Args) -> Self {
        Self {
            top: args.top,
            repo_root: args.repo_root,
            rust_manifest: args.rust_manifest,
            rust_root: args.rust_root,
            swift_package: args.swift_package,
            swift_root: args.swift_root,
            exclude: args.exclude,
            rust_coverage_ignore_regex: args.rust_coverage_ignore_regex,
            report_json: args.report_json,
            cargo_crap: args.cargo_crap,
            cargo_llvm_cov: args.cargo_llvm_cov,
            swift: args.swift,
            swift_analyzer_package: args.swift_analyzer_package,
            llvm_cov: args.llvm_cov,
            llvm_profdata: args.llvm_profdata,
        }
    }
}

fn main() {
    std::process::exit(run(Args::parse().into()));
}
