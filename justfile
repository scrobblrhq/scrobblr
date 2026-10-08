# Builds compile the query macros against the committed .sqlx/ cache, never
# against whatever schema the local database has, so local runs and CI agree.
# `just sqlx-prepare` refreshes the cache after a query changes.
export SQLX_OFFLINE := "true"

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

# apply pending migrations to DATABASE_URL (`just migrate status` lists them)
migrate *args:
    cargo run -q -p worker -- migrate {{args}}

# unit tests (no database needed); also regenerates packages/types
test:
    cargo test --workspace

# fails when `just test` regenerated TypeScript bindings that aren't committed
types-check:
    @test -z "$(git status --porcelain -- packages/types/src/generated)" \
        || (git status --short -- packages/types/src/generated; \
            echo "packages/types is stale: commit what \`just test\` generated"; exit 1)

# database tests: need Postgres and Redis (devenv up); each test creates and drops its own database
test-db:
    cargo test --workspace -- --ignored

# refresh .sqlx/ after adding or changing a query (needs a migrated DATABASE_URL)
sqlx-prepare:
    SQLX_OFFLINE=false cargo sqlx prepare --workspace

# fails when .sqlx/ doesn't match the queries (needs a migrated DATABASE_URL)
sqlx-check:
    SQLX_OFFLINE=false cargo sqlx prepare --workspace --check

# release build of both binaries
build:
    cargo build --release -p api -p worker

# CI's first job: everything that needs no services
ci: fmt-check lint test types-check

# CI's database job: migrate DATABASE_URL, run the database tests, check .sqlx/
ci-db: migrate test-db sqlx-check
