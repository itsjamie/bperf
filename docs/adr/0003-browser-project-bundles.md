# ADR 0003: Browser project bundles

Status: accepted · 2026-07-24

## Context

A benchmark should import the subject the same way the project source does.
Serving TypeScript files directly works for relative ESM imports, but real
projects also use package export maps, CommonJS dependencies, path aliases, and
TypeScript syntax. hls.js's MP4 utilities demonstrate the boundary: their source
imports ESM packages and the CommonJS `url-toolkit` package through another
hls.js module.

Two integrations were considered.

1. Rewrite package specifiers while serving each source module independently,
   adding CommonJS interop and project-resolution rules to the server.
2. Bundle the benchmark entry for a browser and use the bundler's metadata as
   the measured input graph.

The first option duplicates a growing part of a browser bundler. It also turns
package conditions, CommonJS interop, aliases, and dynamic imports into
separate policies that can drift.

## Decision

`project-modules` creates an in-memory, browser-targeted ESM bundle when the
benchmark host starts. `bperf/browser` remains a virtual external import
supplied by the host. The same bundle is served to Chromium, Firefox, and
WebKit.

The variant identity includes every source reported in the bundle metadata. It
also includes package manifests, lockfiles, and TypeScript or JavaScript
configuration found between each input and the workspace root. These files can
change resolution or emitted semantics even when the imported source text does
not change.

The common benchmark API does not expose a build command or bundler options.
Projects that require custom plugins, generated code, opaque runtime imports,
or production-only transforms use the advanced adapter protocol until a
project-level contract can represent those semantics without
benchmark-specific plumbing.

## Consequences

- Normal project imports work without author-supplied loaders or build steps.
- ESM and CommonJS dependencies use one resolution and interop implementation.
- The measured source checkpoint contains implementation files and the
  configuration that selected them.
- Bundle generation remains outside all measured regions.
- The bundle carries an inline source map so native profiles retain source
  attribution where an engine supports it.
- Bundler behavior is measurement infrastructure and must participate in
  compatibility identity.
