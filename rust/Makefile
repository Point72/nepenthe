
.PHONY: requirements develop build
requirements:  ## install required dev dependencies
	rustup component add rustfmt
	rustup component add clippy
	cargo install --locked --force cargo-nextest
	cargo install --force cargo-llvm-cov

develop: requirements  ## install required dev dependencies

build:  ## build release
	cargo build --release --all-features
	$(MAKE) aliases

.PHONY: aliases
aliases:  ## create np/npb symlinks next to the built nepenthe binary (unix)
	@for d in target/release target/debug; do \
		if [ -x "$$d/nepenthe" ]; then \
			ln -sf nepenthe "$$d/np"; \
			ln -sf nepenthe "$$d/npb"; \
			echo "linked $$d/{np,npb} -> nepenthe"; \
		fi; \
	done

.PHONY: lint lints fix format
lint:  ## run Clippy for linting, rustfmt for autoformat checks
	cargo clippy --all-features
	cargo fmt --all -- --check
# alias
lints: lint

fix:  ## fix code with rustfmt
	cargo fmt --all
#alias
format: fix

.PHONY: check checks
check:
	cargo check --all-features
# alias
checks: check

.PHONY: test tests test-ci tests-ci
test:  ## run the tests
	cargo llvm-cov nextest --cobertura --output-path  junit.xml

# alias
tests: test

coverage:
	cargo llvm-cov nextest --lcov --output-path coverage

.PHONY: dist publish
dist:  ## build release artifacts (standalone binary + libraries)
	cargo build --release --all-features
	$(MAKE) aliases

MUSL_TARGET ?= x86_64-unknown-linux-musl

.PHONY: dist-static
dist-static:  ## build a fully static standalone binary (musl, via cargo-zigbuild)
	rustup target add $(MUSL_TARGET)
	cargo zigbuild --release --locked --bin nepenthe --target $(MUSL_TARGET)
	@d="target/$(MUSL_TARGET)/release"; \
		ln -sf nepenthe "$$d/np"; ln -sf nepenthe "$$d/npb"; \
		echo "static binary: $$d/nepenthe"; \
		file "$$d/nepenthe" | sed 's/^/  /'

publish:  ## publishing is disabled (publish = false in Cargo.toml)
	@echo "nepenthe-core is not published to crates.io (publish = false in Cargo.toml)."
	@echo "Configure an approved internal registry before enabling this target."
	@exit 1

# Thanks to Francoise at marmelab.com for this
.DEFAULT_GOAL := help
help:
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-30s\033[0m %s\n", $$1, $$2}'

print-%:
	@echo '$*=$($*)'
