.PHONY: all setup check lint build test clean

all: check lint build

setup:
	rustup update
	rustup component add clippy rustfmt

check:
	@echo "🔍 Checking code for compilation errors..."
	cargo check

lint:
	@echo "🧹 Running linter and formatter..."
	cargo fmt
	cargo clippy -- -D warnings

build:
	@echo "🏗️ Building release version..."
	cargo build --release

test:
	@echo "🧪 Running tests..."
	cargo test

clean:
	@echo "🗑️ Cleaning artifacts..."
	cargo clean

run:
	@echo "🚀 Running the application..."
	cargo run