# ADR 0010: Bundle benchmark projects with Rolldown in Rust

Status: accepted · 2026-07-28

## Context

Managed benchmarks need one browser-targeted ESM bundle that handles
TypeScript, TSX, package exports, CommonJS interop, path aliases, dynamic
imports, tree shaking, and source maps. The original implementation delegated
that work to esbuild inside the Node benchmark host. This made Node responsible
for both serving and compilation, added platform-specific esbuild packages to
the embedded runtime, and prevented the bundling boundary from moving into the
Rust executable.

Four replacements were considered:

1. Use Rolldown's Node API, retaining the existing host boundary.
2. Invoke esbuild's standalone executable from Rust.
3. Compose `swc_bundler`, loaders, transforms, and resolution in bperf.
4. Embed Rolldown's Rust crate behind a bperf-owned project-bundle interface.

The Node API would change the bundler without reducing the Node requirement. A
standalone esbuild process would preserve more output compatibility, but would
add a companion executable and another installation protocol. SWC exposes the
necessary pieces, but bperf would own the package-resolution and transform
composition that a complete bundler should hide.

## Decision

The Rust `project_modules` module owns benchmark bundling through Rolldown.
Callers provide a workspace root, benchmark entry, and output directory. They
receive paths for one materialized browser bundle, its metadata, and the
canonical project source graph; Rolldown options do not cross that boundary.

The common bundle is browser-targeted ESM with `bperf/browser` external,
ES2022 lowering, tree shaking, CommonJS interop, automatic tsconfig discovery,
disabled code splitting, and an inline source map. Disabling code splitting
keeps the host contract to one JavaScript response while retaining statically
resolvable dynamic imports in the bundle graph.

Rolldown's watched files form the module graph. bperf adds package manifests,
lockfiles, and TypeScript or JavaScript configuration between each module and
the workspace root because those files can change resolution or emitted
semantics. Every retained path must resolve inside the benchmark workspace.

The generated metadata records relative source paths and the Rolldown version
resolved by `Cargo.lock`. Both the JavaScript bundle and metadata participate
in variant identity. The TypeScript benchmark host validates the metadata,
serves the supplied bundle, and reports its source graph; it no longer invokes
a bundler.

This supersedes the Node-owned bundling portions of ADR 0003 and ADR 0009. Node
continues to host the loopback fixture server and execute the pinned Playwright
installer until those responsibilities move separately.

## Consequences

- Managed project bundling no longer requires an npm bundler or
  platform-specific esbuild package.
- Cargo builds compile Rolldown into the bperf executable.
- A bundler or bundle-option change necessarily changes variant identity.
- Release runtime archives retain the TypeScript host and Playwright packages,
  but no longer contain esbuild.
- Build time and executable size increase because Rolldown and Oxc are linked
  into bperf.
- Bundler changes require cross-engine execution tests and fresh measurement
  baselines; output from different bundlers is not assumed comparable.
