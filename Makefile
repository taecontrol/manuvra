.PHONY: fmt lint test crap live live-all live-hover live-keyboard live-natural-done live-observation live-resume-dispositions live-run-lifecycle live-self-test live-scroll live-money-scroll

CRAP_REPORT ?= target/crap-report.json
CRAP_GATE_MANIFEST := tools/crap-gate/Cargo.toml

fmt:
	cargo fmt --all --check
	cargo fmt --manifest-path $(CRAP_GATE_MANIFEST) --all --check

lint:
	cargo clippy --workspace --all-targets --all-features -- -D warnings
	cargo clippy --locked --manifest-path $(CRAP_GATE_MANIFEST) --all-targets --all-features -- -D warnings

test:
	cargo test --workspace --all-targets --all-features --locked
	cargo test --locked --manifest-path $(CRAP_GATE_MANIFEST) --all-targets --all-features

crap:
	mkdir -p $(dir $(CRAP_REPORT))
	cargo run --locked --manifest-path $(CRAP_GATE_MANIFEST) -- --repo-root . --rust-manifest Cargo.toml --rust-root crates --platform-profiles tools/crap-gate/platform-profiles.json --report-json $(CRAP_REPORT)

# Deterministic checks of the live-suite harnesses; they need no provider key, browser, or Money.
live-self-test:
	bash scripts/live/fixture-port-self-test.sh
	/bin/bash scripts/live/money-journey-matrix.sh --self-test
	/bin/bash scripts/live/money-journey-matrix.sh --runtime-self-test
	bash scripts/live/money-scroll.sh --self-test
	python3 scripts/live/scroll-matrix.py --self-test-detectors
	python3 scripts/live/keyboard-matrix.py --self-test-detectors
	python3 scripts/live/hover-journey-check.py --self-test
	bash scripts/live/natural-done.sh --self-test

live:
	/bin/bash scripts/live/money-journey-matrix.sh

live-hover:
	bash scripts/live/hover-reveal.sh

live-money-scroll:
	bash scripts/live/money-scroll.sh

live-scroll:
	python3 scripts/live/scroll-matrix.py

live-keyboard:
	python3 scripts/live/keyboard-matrix.py

live-natural-done:
	bash scripts/live/natural-done.sh

live-observation:
	bash scripts/live/browser-observation.sh

live-resume-dispositions:
	bash scripts/live/resume-dispositions.sh

live-run-lifecycle:
	bash scripts/live/run-lifecycle.sh

# Sequential even under -j: the Money suites share fixture port 4351.
live-all:
	$(MAKE) live
	$(MAKE) live-observation
	$(MAKE) live-natural-done
	$(MAKE) live-resume-dispositions
	$(MAKE) live-run-lifecycle
	$(MAKE) live-hover
	$(MAKE) live-keyboard
	$(MAKE) live-scroll
	$(MAKE) live-money-scroll
