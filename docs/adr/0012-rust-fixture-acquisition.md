# ADR 0012: Acquire benchmark fixtures in Rust

Status: accepted · 2026-07-28

## Context

After bundling and loopback serving moved into Rust, managed discovery still
started a TypeScript process to acquire fixtures. That process resolved local
paths, downloaded HTTP and HTTPS sources, followed redirects, inferred content
types, wrote content-addressed bodies, and finalized the fixture lock.

The resolver was the last reason `bperf run` and `bperf confirm` needed Node.
It also duplicated the fixture descriptor and lock schema already validated by
the Rust host.

Three designs were considered:

1. Keep the direct TypeScript resolver as a small, isolated sidecar.
2. Invoke an external downloader such as curl while Rust owns local files and
   lock creation.
3. Put acquisition, caching, and lock validation behind one Rust fixture
   module.

The TypeScript process preserved the existing implementation but retained a
runtime prerequisite and a process protocol for one pre-trial operation. An
external downloader would add platform and version differences while exposing
redirect, header, proxy, TLS, and exit-code details to orchestration.

## Decision

The Rust `fixtures` module owns the complete fixture descriptor and lock
contract. Managed discovery passes it the canonical project root, benchmark
module, object store, lock path, and browser-produced descriptors. Callers
receive only the canonical lock and identity-file paths.

Local sources are resolved relative to the benchmark module after symlinks and
must remain inside the project root. They are reacquired on every discovery so
source edits produce a new content-addressed object.

HTTP and HTTPS sources are acquired with ureq's blocking client. Acquisition
uses HTTPS certificate validation, environment proxy settings, gzip decoding,
and at most ten redirects. The lock records both the normalized source URI and
the final response URI, response content type, SHA-256, and byte length. A
remote descriptor already present in the lock reuses its pinned body and fails
if that object is missing or corrupt; it is not silently refreshed.

The same module strictly loads and validates locks for the Rust benchmark host.
Descriptor canonicalization and body-integrity rules therefore have one
implementation.

This supersedes the fixture-acquisition portions of ADR 0007, ADR 0009, and
ADR 0011.

## Consequences

- `bperf run` and `bperf confirm` no longer accept `--node`, inspect
  `BPERF_NODE`, or start a Node process.
- Local containment, remote redirects, pinned reuse, object integrity, and lock
  serialization have direct Rust regression coverage.
- The TypeScript fixture resolver and project-file helper are removed from the
  embedded runtime and release package.
- ureq, rustls, ring, and Mozilla web PKI roots are linked into bperf for
  portable HTTPS acquisition.
- Node remains necessary for `bperf browsers install` and for npm
  materialization after a source-only Cargo installation. Replacing the
  Playwright installer and registry package is the remaining production step
  toward complete Node removal.
