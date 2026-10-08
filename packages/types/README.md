# @scrobblr/types

Shared TypeScript types for Scrobblr, generated from the Rust API models
(`crates/shared`) via [ts-rs], for the TypeScript clients (the browser
extension and the web app). It covers those models only; the request and
response types handlers define are in the OpenAPI spec, `openapi.json` at the
repo root, which is meant to replace this package.

## How it stays in sync

The `.ts` files under `src/generated/` are produced by ts-rs: running
`cargo test -p shared` at the repo root regenerates them (the export dir is set
by `TS_RS_EXPORT_DIR` in `.cargo/config.toml`). **Never edit them by hand.**

`src/generated/index.ts` is an auto-generated barrel — `bun run gen-index`
(part of `bun run build`) rebuilds it from whatever files exist, so a new model
can't be forgotten.

## Consuming it

From npm (the extension, the web app, contributors):

```sh
bun add @scrobblr/types
```

```ts
import type { ScrobbleRich, UserProfile } from "@scrobblr/types";
```

## Publishing

Published to the public npm registry by
[`.github/workflows/types-release.yml`](../../.github/workflows/types-release.yml).

1. Change a model in `crates/shared`, run `cargo test -p shared`, commit the
   regenerated `src/generated/` files.
2. Tag a release: `git tag types-v0.2.0 && git push origin types-v0.2.0`
   (or run the workflow manually with a version).

The workflow regenerates the bindings and **fails if the committed types are
stale**, then builds and publishes. Bump the version following semver —
removing or renaming a field is a breaking change for consumers.

Requires an `NPM_TOKEN` repo secret (an npm automation/granular token with
publish rights to the `@scrobblr` scope).

[ts-rs]: https://github.com/Aleph-Alpha/ts-rs
