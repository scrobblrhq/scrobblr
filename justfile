# formatting
fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all --check

# linting
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# automatically fix clippy warnings
lint-fix:
	cargo clippy --workspace --all-targets --fix --allow-dirty --allow-staged

# type checking
check:
    cargo check --workspace

# build
build:
    turbo run build

# all checks
ci: fmt-check lint check build