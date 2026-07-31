# ADR 0011: Serve managed benchmarks from Rust

Status: accepted · 2026-07-28

## Context

After project bundling moved to Rolldown in Rust, the TypeScript sidecar still
opened the loopback HTTP server used by discovery and every trial. It served the
generated bundle, transformed and served `bperf/browser` through an import map,
and implemented locked fixture responses, byte ranges, and paced streams.

That boundary kept Node in the measurement execution path even though it no
longer owned browser launch, capture, or project bundling. It also split the
serving contract across Rust-generated bundle metadata and TypeScript host
behavior.

Three designs were considered:

1. Keep the Node HTTP host and only remove its bundling responsibility.
2. Launch a general-purpose external static-file server from each adapter.
3. Put a benchmark-specific loopback host behind one Rust module and expose it
   through a hidden command of the same bperf executable.

The existing host preserved behavior but did not reduce the runtime requirement.
A general static server would still need a second process and would expose
fixture-lock, range, streaming, readiness, and validation details to its caller.

## Decision

The Rust `benchmark_host` module owns loopback serving for one materialized
benchmark. Its caller supplies a validated `BrowserProjectBundle` and fixture
lock. The module hides address selection, concurrent request handling, page and
bundle routes, fixture descriptor lookup, content integrity validation, byte
ranges, paced streaming, and shutdown.

Managed discovery starts this host in process. Generated measurement variants
start the same bperf executable with a hidden `__benchmark-host` command. The
command validates the bundle metadata and emits the existing versioned
stdio-readiness record, so the generic variant adapter remains unaware of HTTP
or fixture details.

Rolldown aliases `bperf/browser` to the embedded browser authoring module and
inlines it into the generated ESM bundle. The served page therefore needs no
import map or separately transformed SDK route.

Node remains responsible only for acquiring local or remote fixtures and
writing their content-addressed lock before resolved discovery, plus invoking
Playwright's browser installer when requested. It is not started to serve
discovery or trial traffic.

This supersedes the Node-host and external-`bperf/browser` portions of ADR 0003,
ADR 0007, ADR 0009, and ADR 0010.

## Consequences

- Discovery and every measurement trial use the Rust host; Node is absent from
  the serving and browser-capture path.
- Generated variants depend only on the current bperf executable, materialized
  bundle and metadata, and locked fixture evidence.
- Fixture bodies are integrity-checked before the host becomes ready, keeping
  hashing outside measured fixture responses.
- Eight persistent request workers cover normal per-origin browser concurrency
  without spawning a thread for each request.
- The embedded Node runtime still includes the direct TypeScript fixture
  resolver, browser authoring source, Playwright packages, and pinned registry.
- Complete Node removal still requires replacing fixture acquisition and the
  Playwright installer.
