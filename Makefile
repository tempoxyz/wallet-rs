.PHONY: build release clean check test fix install uninstall run coverage

build:
	cargo build

# make run ARGS="http://localhost:3000/api/data"
run:
	cargo run -q -p tempo-wallet -- $(ARGS)

release:
	cargo build --release

install: release
	mkdir -p $(HOME)/.tempo/bin
	cp target/release/tempo-wallet $(HOME)/.tempo/bin/tempo-wallet
	cp target/release/tempo-request $(HOME)/.tempo/bin/tempo-request
	cp target/release/tempo-cards $(HOME)/.tempo/bin/tempo-cards
	chmod +x $(HOME)/.tempo/bin/tempo-wallet $(HOME)/.tempo/bin/tempo-request $(HOME)/.tempo/bin/tempo-cards
	@echo ""
	@echo "Installed:"
	@$(HOME)/.tempo/bin/tempo-wallet --version
	@$(HOME)/.tempo/bin/tempo-request --version
	@$(HOME)/.tempo/bin/tempo-cards --version

uninstall:
	rm -f $(HOME)/.tempo/bin/tempo-wallet $(HOME)/.tempo/bin/tempo-request $(HOME)/.tempo/bin/tempo-cards

clean:
	cargo clean

# Run all tests (uses mocks, no network required)
test:
	cargo test --workspace --all-features --locked

# Full local parity with CI lint+test gates.
# Requires `typos` and `cargo-deny` to be installed.
check:
	cargo +nightly fmt --all -- --check
	cargo +nightly clippy --workspace --all-targets --all-features --locked -- -D warnings
	cargo test --workspace --all-features --locked
	RUSTDOCFLAGS='-D warnings' cargo doc --workspace --all-features --no-deps --locked
	typos
	cargo deny check

fix:
	cargo +nightly fmt --all
	cargo clippy --fix --allow-dirty --allow-staged

# Generate coverage locally (requires cargo-llvm-cov and llvm-tools-preview)
# Install once: `rustup component add llvm-tools-preview` and `cargo install cargo-llvm-cov`
coverage:
	cargo llvm-cov --all-features --workspace --fail-under-lines 85 --lcov --output-path lcov.info
