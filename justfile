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

# apply pending migrations (`just migrate status` lists them). Offline, so it
# compiles against .sqlx/ before the database has the schema the queries expect
migrate *args:
    SQLX_OFFLINE=true cargo run -q -p worker -- migrate {{args}}

# unit tests (no database needed)
test:
    cargo test --workspace

# database tests: need Postgres (devenv up); each test creates and drops its own database
test-db:
    cargo test --workspace -- --ignored

# build
build:
    turbo run build

# all checks
ci: fmt-check lint check test build