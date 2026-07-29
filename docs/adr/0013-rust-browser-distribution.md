# ADR 0013: Own browser distribution in Rust

Status: accepted · 2026-07-28

## Context

Rust already owned bundling, fixture acquisition, loopback serving, browser
processes, automation, and capture. Two production operations still executed
Node.js: first-use npm materialization of the Playwright packages and
`bperf browsers install` invoking Playwright's CLI. CI, release packaging, and
installation smoke tests also used Node.

The packages were retained for a narrow reason. `playwright-core` supplied the
browser revisions, platform overrides, download URL templates, cache naming,
archive executable paths, and Linux dependency lists. The browser adapters did
not use Playwright's JavaScript API.

The npm registry signs each `playwright-core` package record and publishes a
SHA-512 tarball integrity value plus provenance. That authenticates
`browsers.json` and the package source, but Playwright does not publish a
signed, declarative browser-download manifest. URL templates, executable paths,
and native dependencies remain executable package source, while the browser
ZIPs have no upstream SHA-256 or SHA-512 manifest. Fetching package metadata at
install time would therefore add a mutable availability boundary without
authenticating the browser archives themselves.

Three designs were considered:

1. Continue embedding the production Playwright packages and limit Node to
   installation.
2. Download a generated bperf runtime manifest from each GitHub release.
3. Generate an authenticated, reviewed static registry from the signed package,
   embed it in Rust, and install the upstream browser archives directly.

The first design kept a large runtime and a second language solely to interpret
static distribution data. A separately downloaded manifest introduced another
availability, checksum, and compatibility boundary before browser installation
could begin.

## Decision

The `bperf-build playwright-registry` job downloads one `playwright-core`
package record, accepts only an ECDSA signature from a pinned npm key, and
verifies the tarball against the signed SHA-512 integrity. It reads
`browsers.json` and a deliberately narrow static subset of `coreBundle.js`
without executing package JavaScript. The generated, reviewed JSON contains the
provider version, source authentication evidence, browser revisions, platform
overrides, mirror-relative download paths, archive executable paths, cache
directories, and supported Linux dependency groups.

`bperf-runtime` embeds that file and exposes browser artifacts through one
validated lookup. Host detection, mirror selection, cache conventions,
installation markers, downloading, and extraction remain hidden behind the
runtime interface. The installer never fetches or interprets mutable registry
metadata.

`bperf browsers install` downloads archives over HTTPS with the Rust HTTP
client, tries the provider mirrors in order, rejects unsafe or oversized ZIP
entries, preserves Unix permissions and contained symlinks, verifies the
expected executable, and atomically activates the completed browser directory.
Existing compatible Playwright cache directories remain reusable.

On supported Ubuntu and Debian releases, `--with-deps` installs the exact
package union for the selected engines. Other operating systems either need no
separate package step or fail explicitly when automatic dependency
installation is unsupported.

The TypeScript authoring API and JavaScript browser workload are source assets
compiled into the Rust crates. They execute only in benchmark browsers.
Rolldown consumes the authoring API through a temporary private module during
bundling; no external runtime tree is materialized.

Release packaging, registry generation, and installed-package verification live
in the `bperf-build` Rust crate. CI has no JavaScript runtime setup or npm step.
Its registry contract regenerates the checked-in file from the authenticated
package and fails on any byte of drift.

This supersedes the runtime-materialization and Playwright-CLI portions of ADR
0009, plus the remaining-production-step consequence in ADR 0012.

## Consequences

- Node.js and npm are not required to build, test, package, install, or run
  bperf.
- Cargo source installs and downloaded release archives have the same
  single-executable boundary.
- Browser archives remain explicit post-install downloads and continue to use
  Playwright's patched builds, including WebKit.
- Updating the pinned Playwright version requires running the authenticated
  generator, reviewing the static registry diff, then passing every platform
  package and live-browser contract.
- npm signing-key rotation is an explicit maintenance event because the
  generator will reject packages signed only by an unpinned key.
- Playwright's browser ZIPs still lack an upstream signed SHA-256 or SHA-512
  manifest. The embedded paths are authenticated through the package, while
  archive transport relies on HTTPS. A future release process may add
  bperf-owned hashes after independently acquiring each platform archive.
- bperf owns archive extraction and cache activation security rather than
  delegating those decisions to a package-manager dependency.
