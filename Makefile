CARGO ?= cargo

.PHONY: build fmt fmt-check lint test check

build:
	$(CARGO) build --workspace

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

lint:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

test:
	$(CARGO) test --workspace

check: fmt-check lint test
