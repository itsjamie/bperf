# ADR 0009: Release one self-contained executable per native target

Status: superseded in part by ADR 0013 · 2026-07-27

## Context

The Rust browser adapters removed Node from browser capture, but managed
benchmarks still require the TypeScript benchmark host, esbuild, and the pinned
Playwright registry. A normal Cargo installation copies executable targets
only. Installing the Rust binary without those runtime files would succeed and
then fail when the user ran a managed benchmark.

Tagged releases also need to prove the installed layout rather than the source
checkout. A release test that finds `sidecar/` through its compile-time
repository path does not validate what a downloaded user receives.

Three distribution designs were considered:

1. Publish a multi-file archive and leave Cargo installs unsupported.
2. Install one Rust executable, then download a second runtime archive on first
   use.
3. Embed the locked benchmark runtime in the Rust executable and materialize it
   beside the executable when first needed.

The archive-only design gives manual downloads a complete installation but
makes a successful `cargo install` misleading. A second runtime download keeps
the executable small, but makes first use depend on another release request and
requires a separate checksum, cache, and recovery protocol.

## Decision

`bperf-runtime` embeds a compressed, versioned archive containing the benchmark
host, authoring module, project bundler, package manifests, and production
`esbuild`, `playwright`, and `playwright-core` packages. Release packaging
supplies a production-only runtime to the crate build. Source Cargo builds embed
the runtime sources and exact package lock; if production packages were not
available during compilation, first materialization runs `npm ci --omit=dev`
before activation.

Materialization writes to a temporary directory below
`bperf-runtime/<version>/` beside the executable and renames the completed
`sidecar/` directory into place. Concurrent processes may race to create the
same immutable version, but neither can expose a partial runtime. An explicitly
configured `BPERF_SIDECAR_DIR` still fails if invalid instead of silently
falling back.

Debug builds may use the repository's sidecar. Release builds do not consult
their compile-time source path, so package validation cannot borrow files from
the checkout.

`bperf browsers install` invokes the embedded runtime's pinned Playwright CLI.
Browser binaries and Linux system packages are not embedded: they are much
larger than bperf, vary by host, and already have a versioned installer. The
`--with-deps` option delegates operating-system dependency installation to
Playwright.

Release archives use
`bperf-<version>-<rust-target>.tar.gz` and contain a same-named top-level
directory. The Cargo manifest records the corresponding cargo-binstall URL and
binary path. Stock Cargo can install a tagged source revision; the embedded
runtime ensures that copying only the executable remains a valid installation.

The CI package contract covers x86-64 Linux and Windows, Apple Silicon macOS,
and Intel macOS. Each job:

1. creates the release archive;
2. installs only its executable into a clean Cargo root;
3. installs all pinned browsers through that executable;
4. runs the complete doctor contract; and
5. measures a managed TypeScript benchmark; then
6. repeats installation through stock Cargo and proves its source-runtime
   provisioning with Chromium.

A tag publishes only after the fast, per-engine, cross-engine, and package
contracts succeed. The tag must equal `v<Cargo package version>`. The GitHub
release contains every native archive and a `SHA256SUMS` file.

## Consequences

- Manual release downloads, Cargo-installed binaries, and package tests use the
  same executable/runtime boundary.
- Release binaries are larger because they contain the production benchmark
  runtime once.
- A source Cargo install needs Node and npm on first use when its build did not
  have production packages available.
- Browser downloads remain an explicit post-install step.
- Updating Playwright or esbuild changes both benchmark identity and the
  embedded release payload, so the package contract must pass on every release
  target.
