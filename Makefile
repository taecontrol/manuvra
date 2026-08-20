.PHONY: fmt lint test crap verify-proof installed-proof

CRAP_REPORT ?= target/crap-report.json
PROOF_PREFIX ?= $(CURDIR)/target/installed-proof-prefix
PROOF_ROOT ?= $(CURDIR)/target/installed-proof
PROOF_ATTEMPTS ?= 50
PROOF_CERTIFICATE ?= proof/exhaustive-crap-certificate.json

fmt:
	cargo fmt --all --check

lint:
	cargo clippy --workspace --all-targets --all-features -- -D warnings

test:
	cargo test --workspace --all-targets --all-features --locked

crap:
	mkdir -p $(dir $(CRAP_REPORT))
	cargo run --locked --manifest-path tools/crap-gate/Cargo.toml -- --repo-root . --rust-manifest Cargo.toml --rust-root crates --exclude 'manuvra-cli/tests/**' --exclude 'manuvra-chrome/tests/**' --exclude 'manuvra-runtime/tests/**' --report-json $(CRAP_REPORT)

verify-proof:
	scripts/verify-proof-certificate.sh $(PROOF_CERTIFICATE)

installed-proof:
	scripts/run-installed-proof.sh --prefix $(PROOF_PREFIX) --evidence-root $(PROOF_ROOT) --attempts $(PROOF_ATTEMPTS)
