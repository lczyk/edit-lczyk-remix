.SUFFIXES:

help:
	@echo "Available targets:"
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-20s\033[0m %s\n", $$1, $$2}'

.PHONY: sync-version
sync-version:  ## Sync crate Cargo.toml versions from VERSION (source of truth)
	@v=$$(awk '/^[[:space:]]*#/ {next} /^[[:space:]]*$$/ {next} {gsub(/[[:space:]]/,""); print; exit}' VERSION); \
	if [ -z "$$v" ]; then echo "VERSION has no version line" >&2; exit 1; fi; \
	for crate in edit lsh-defs; do \
	awk -v v="$$v" ' \
	  /^version = ".*"[[:space:]]*#[[:space:]]*source:[[:space:]]*\/VERSION/ { \
	    print "version = \"" v "\"  # source: /VERSION (synced by `make sync-version`; do not edit by hand)"; next \
	  } \
	  { print } \
	' crates/$$crate/Cargo.toml > crates/$$crate/Cargo.toml.tmp && mv crates/$$crate/Cargo.toml.tmp crates/$$crate/Cargo.toml; done

.PHONY: build
build: sync-version  ## Release build (stable toolchain, larger binary)
	cargo build --release
	@if command -v upx >/dev/null 2>&1; then \
		upx target/release/edit || echo "upx failed, skipping compression"; \
	fi

.PHONY: du
du: build  ## Show release binary size
	du -h target/release/edit

.PHONY: install
install: sync-version  ## Install the edit binary (debug build, sanity feature on) into ~/.cargo/bin
	cargo install --debug --features sanity --path crates/edit --force
	ln -sf edit "$${CARGO_INSTALL_ROOT:-$$HOME/.cargo}/bin/eat"

# Every package, every feature except one: stdext's `single-threaded` swaps the
# scratch arenas for a `static mut` pair, which is only sound in a
# single-threaded program -- and the test harness runs tests on threads, where
# it aborts with an arena OOM. So --all-features is not usable here; this list
# is every other feature in the workspace. Keep it in step when adding one.
FEATURES := edit/sanity,lsh/sanity,lsh-defs/sanity,gutter/sanity,stdext/sanity

.PHONY: check
check:  ## Fast type-check across the workspace
	cargo check --workspace --all-targets --features $(FEATURES)

.PHONY: clippy
clippy:  ## Clippy with warnings denied (CI bar)
	cargo clippy --workspace --all-targets --features $(FEATURES) -- --deny warnings

.PHONY: test
test:  ## Run the test suite across the workspace
	cargo test --workspace --features $(FEATURES)

# ICU is dlopen'd, so the search tests skip when it can't be loaded. The
# build defaults to the unversioned SONAME, which only exists if the -dev
# package is installed; failing that, point at whatever versioned library
# is present. EDIT_TEST_REQUIRE_ICU turns the skip into a failure so this
# target can't quietly pass having tested nothing.
.PHONY: test-icu
test-icu:  ## Run the test suite with ICU wired up (search tests must run)
	@soname=$$(ldconfig -p 2>/dev/null | sed -n 's/.*\(libicuuc\.so\.[0-9][0-9]*\).*/\1/p' | head -1); \
	i18n=$$(ldconfig -p 2>/dev/null | sed -n 's/.*\(libicui18n\.so\.[0-9][0-9]*\).*/\1/p' | head -1); \
	if [ -z "$$soname" ]; then \
		echo "error: no libicuuc.so.* found via ldconfig."; \
		echo "  install ICU (e.g. 'sudo apt install libicu-dev') and retry."; \
		exit 1; \
	fi; \
	echo "using $$soname / $$i18n"; \
	EDIT_CFG_ICUUC_SONAME=$$soname \
	EDIT_CFG_ICUI18N_SONAME=$$i18n \
	EDIT_TEST_REQUIRE_ICU=1 \
	cargo test --all-features

.PHONY: format
format:  ## Format the workspace with rustfmt
	cargo fmt --all

.PHONY: fmt-check
fmt-check:  ## Verify formatting without modifying files
	cargo fmt --all -- --check

.PHONY: cover
cover:  ## Coverage profile + HTML file (cover.out, cover.html)
	@if ! cargo llvm-cov --version >/dev/null 2>&1; then \
		echo "error: cargo-llvm-cov not installed."; \
		echo "  install: cargo install cargo-llvm-cov && rustup component add llvm-tools-preview"; \
		exit 1; \
	fi
	cargo llvm-cov --all-features --lcov --output-path cover.out
	cargo llvm-cov report --html --output-dir target/llvm-cov
	cargo llvm-cov report

.PHONY: cover-open
cover-open: cover  ## Run coverage and open the HTML report in a browser
	cargo llvm-cov --all-features --html --open

.PHONY: clean
clean:  ## Remove the target/ directory
	cargo clean

.PHONY: docs-build
docs-build:  ## Build the mdBook knowledge base into doc/book/
	@if ! command -v mdbook >/dev/null 2>&1; then \
		echo "error: mdbook not installed."; \
		echo "  install: cargo install mdbook"; \
		exit 1; \
	fi
	mdbook build doc

.PHONY: docs-serve
docs-serve:  ## Serve the mdBook knowledge base with live reload
	@if ! command -v mdbook >/dev/null 2>&1; then \
		echo "error: mdbook not installed."; \
		echo "  install: cargo install mdbook"; \
		exit 1; \
	fi
	mdbook serve doc --open

.PHONY: docs-clean
docs-clean:  ## Remove the built mdBook output
	rm -rf doc/book

.PHONY: verify
verify: fmt-check clippy test  ## Run the full pre-commit gate (fmt, clippy, test)
	@echo "All checks passed."
