# ADR 0015: Make SQLite the canonical structured store

Status: accepted - 2026-07-31

Supersedes ADR 0014 for structured records. ADR 0014 still governs publication
of native browser captures, source objects, fixture bodies, and other external
payload files.

## Context

The original `.bperf` layout represented each domain as JSON documents and
JSONL journals. That layout was inspectable, but answering a history query
required directory discovery, repeated parsing, and, when a cycle was
selected, reopening its measurement set and hashing retained browser and source
payloads. Lazy loading shortened initial TUI startup but moved the same work
onto keyboard interaction.

Three designs were considered:

1. Keep JSON canonical and add more in-memory caches and lazy readers.
2. Keep JSON canonical and maintain a derived SQLite history index.
3. Make one SQLite database canonical for structured state while retaining
   large native payloads as content-validated files.

More caching would reduce repeated work only within one process and would
leave every caller responsible for reconstructing the same joins. A derived
index would create two authorities and require invalidation and repair rules
for every state transition. Those rules would duplicate knowledge already
owned by the measurement, comparison, baseline, and lineage Modules.

## Decision

`.bperf/bperf.sqlite3` is the authority for structured bperf state. It stores
measurement definitions, schedules, sampling decisions, environments, trial
records, artifact descriptors and retention decisions, comparison reports,
baseline and lineage events, source-state metadata, compact history evidence,
and managed-run receipts.

The database Module owns connection policy, schema versioning, WAL mode,
durability, short-lived readers, immutable document publication, ordered event
streams, and write transactions. Domain Modules continue to own their record
schemas, identifiers, and semantic validation. They do not expose SQLite
connections or statements to the TUI.

Native CPU profiles, flamegraphs, heap snapshots, fixture bodies, frozen
workloads, source bytes, and generated browser-adapter inputs remain files.
Their database records contain the identity and descriptors needed to locate
and validate them. Browsing history reads compact evidence already committed
with each cycle; it does not open or hash those payloads. Explicit artifact
opening remains the boundary that reads a native payload.

Baseline acceptance appends the baseline reference and lineage promotion in
one immediate transaction. A reader uses a short, query-only connection and
does not hold a transaction for the lifetime of the TUI.

Because this storage format has not shipped, there is no compatibility reader
or dual-write period. A database either satisfies the current schema or fails
explicitly.

Standalone JSON remains a presentation format for `--json` output and explicit
exports; there is no command that rematerializes the old persistence layout.

## Consequences

- History startup and selection perform indexed record reads and bounded
  deserialization instead of filesystem scans and payload hashing.
- One transaction can preserve invariants that previously crossed journals,
  especially baseline promotion.
- SQLite is linked into the Rust executable through the bundled library; it
  adds no Node.js, npm, service, or user-installed database dependency.
- Users should relocate the complete store with `--data-dir`. Independent
  per-domain roots cannot participate in cross-domain transactions.
- The database is not a container for large diagnostic payloads. File
  publication and content-address validation remain required at that boundary.
- Directly editing `.bperf` internals is unsupported. Stable inspection
  surfaces are the CLI, TUI, and `--json` output.
- Development stores created before this decision are not imported.
