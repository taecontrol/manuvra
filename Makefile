.PHONY: fmt lint test crap live

CRAP_REPORT ?= target/crap-report.json

fmt:
	cargo fmt --all --check

lint:
	cargo clippy --workspace --all-targets --all-features -- -D warnings

test:
	cargo test --workspace --all-targets --all-features --locked

crap:
	mkdir -p $(dir $(CRAP_REPORT))
	cargo run --locked --manifest-path tools/crap-gate/Cargo.toml -- --repo-root . --rust-manifest Cargo.toml --rust-root crates --report-json $(CRAP_REPORT)

live:
	bash scripts/live-slice3.sh
