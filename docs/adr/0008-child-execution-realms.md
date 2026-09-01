# ADR 0008: Preserve child execution realms as native capture scopes

Status: accepted · 2026-07-26

## Context

A browser workload can execute benchmark code in its page, dedicated workers,
and same-origin or cross-origin iframes. Capturing only the page's profiler
session silently loses worker evidence in Chromium and WebKit. Treating the
main page as the only benchmark-owned URL also removes cross-origin loopback
iframe frames during profile attribution.

The engines do not expose the same diagnostic boundary. Chromium creates
separate CDP sessions for dedicated workers and out-of-process iframes, while
in-process frames remain in the page session. WebKit exposes dedicated workers
through nested worker inspector sessions and includes iframe samples in the
page ScriptProfiler session. Firefox's Gecko Profiler and selected
MemoryActor cover the browser context rather than one independently
snapshotable JavaScript realm.

Three interface designs were considered:

1. Normalize every engine into one synthetic CPU profile and one synthetic
   heap snapshot.
2. Promote each page, worker, and iframe to a benchmark case or statistical
   stratum.
3. Preserve engine-native diagnostic groups as capture scopes while aggregating
   their scalar metrics into the existing trial.

Synthetic native payloads would invent merge semantics for formats that are
valuable precisely because they remain browser-native. Making process topology
a statistical dimension would change the meaning and sample count of one
benchmark workload, and the result would vary when an engine moves an iframe
in or out of process.

## Decision

Dedicated workers and iframes are part of the complete capture contract on
Chromium, Firefox, and WebKit. They contribute to the same workload execution,
trial, and benchmark case as the page.

`ArtifactEvidence` carries an adapter-assigned `capture_scope`. Every scope must
contain exactly one native CPU profile, one Speedscope flamegraph, and one
native JavaScript heap snapshot. The artifact Module validates complete groups,
contained paths, nonempty payloads, and immutable digests.

Adapters retain their native boundaries:

- Chromium emits `page` plus a scope for each separately attached dedicated
  worker or out-of-process iframe. In-process iframe evidence remains in
  `page`.
- Firefox emits one `browser-context` scope. Gecko Profiler thread selection
  includes DOM workers, iframe page identities participate in attribution, and
  the heap snapshot remains context-wide.
- WebKit emits `page`, whose ScriptProfiler includes iframe execution, plus a
  scope for each nested dedicated worker.

Loopback HTTP and HTTPS URLs on any origin, along with benchmark-owned blob,
data, and about URLs, qualify for CPU attribution. Browser-internal and
non-loopback URLs do not.

When an engine pauses a new child target, the adapter installs the workload
bootstrap and starts profiling before resuming it. A separately exposed child
target that the adapter cannot capture completely, a worker lost during
capture, or a child created after finalization begins invalidates the attempt.
Shared workers and service workers are outside the common contract.

The per-engine CPU scalar is the sum of benchmark-attributed duration across
its scopes, normalized by batch size. The heap scalar is the sum of live
JavaScript bytes across its scopes after settling. Firefox obtains the
equivalent totals from its single context-wide capture. Capture scopes remain
diagnostic metadata and do not enter comparison keys.

Artifact retention first selects the CPU and heap representative trials for a
case and engine. It then retains every CPU/flamegraph scope from the CPU
representative and every heap scope from the heap representative. This keeps
scope evidence complete without changing the representative observation.

Capture protocol 13, adapter protocol 2, environment schema 7, measurement
schema 5, and artifact-retention schema 2 identify this contract. Earlier
measurement sets require remeasurement.

This decision extends ADR 0005's combined-trial boundary and ADR 0007's
engine-private protocol ownership. It supersedes ADR 0002's assumption that
one artifact of each kind is sufficient for a representative trial; its
selection and resumable-cleanup decisions remain in force.

## Validation

Fast tests cover per-scope completeness, retention across all scopes, profiler
URL attribution, Firefox profiler actor options, nested WebKit worker routing,
and explicit unsupported-target failure.

The real-browser child-realm contract serves a dedicated worker and a
cross-origin loopback iframe, executes named CPU-heavy functions in both, and
retains heap objects in both. One `BrowserLab` must capture positive aggregate
metrics and native flamegraph evidence for both functions on Chromium,
Firefox, and WebKit.

## Consequences

- Benchmark authors can put measured work in dedicated workers and iframes
  without writing engine-specific adapters.
- Comparison remains indexed by case, engine, and metric rather than browser
  process topology.
- A trial may contain more than three native artifacts, and storage grows with
  representative capture-scope count.
- Native payloads remain directly inspectable and are never merged into a
  format an engine did not emit.
- Dynamic child-realm lifecycle errors fail the attempt instead of producing a
  timing-only or page-only sample.
