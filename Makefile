PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin

.PHONY: build install test lint fmt

build:
	cargo build --release --locked

install: build
	mkdir -p $(BINDIR)
	install -m 755 target/release/awm-server $(BINDIR)/awm-server
	install -m 755 target/release/awm-collector $(BINDIR)/awm-collector

test:
	cargo test --workspace --locked

lint:
	cargo clippy --workspace --all-targets -- -D warnings

fmt:
	cargo fmt --all
