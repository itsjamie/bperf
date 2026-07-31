# ADR 0014: Centralize crash-safe local persistence

Status: accepted - 2026-07-29

## Context

Measurement sets, baseline history, optimization lineage, managed benchmark
indices, and fixture locks all persist local state. Their schemas and path
layouts belong to different domain Modules, but they repeated two lower-level
durability rules.

Immutable writers created the final path before writing its content. A process
interruption could therefore leave a partial artifact that subsequent retries
correctly refused to replace. JSONL journals appended directly to the active
file, so an interrupted final record made the entire history unreadable.

Three designs were considered:

1. Correct and retain one persistence helper in each domain crate.
2. Replace every journal with a domain-specific directory of atomic record
   files.
3. Put only the shared durability mechanics behind one narrow storage crate
   while leaving paths, schemas, and semantic validation with their domain
   owners.

Duplicated helpers would keep the failure model and platform behavior spread
across several Modules. Record directories provide strong isolation but would
change existing measurement, baseline, and lineage formats without improving
their domain models.

## Decision

`bperf-storage` owns three local persistence operations:

- immutable publication stages and flushes complete content in the destination
  directory, publishes without clobbering, and accepts an existing winner only
  when its bytes match;
- mutable replacement stages and flushes complete content before an atomic
  rename over the previous version;
- JSONL append locks the journal, removes any unterminated trailing record,
  writes one newline-committed record, and flushes it. Readers ignore an
  unterminated tail so interrupted operations can be inspected and resumed.

On Unix, successful publication and replacement also flush the parent
directory. The crate owns no benchmark schemas, identifiers, path conventions,
or retention policy. Callers exchange serialized records and retain all
domain-specific validation.

Fixture delivery retains the bytes validated against the fixture lock. The
benchmark host receives those validated bytes rather than a storage path it
must reopen and partially revalidate.

This extends the crate graph recorded by ADR 0007 with a lower-level durability
crate; the browser, measurement, and decision ownership boundaries remain
unchanged.

## Consequences

- A final immutable pathname never denotes content that is still being
  constructed.
- Exact concurrent immutable publishers converge; conflicting content remains
  an explicit collision.
- Measurement, baseline, and lineage histories recover safely from a partial
  last record without weakening validation of committed records.
- Durability behavior and its platform-specific details have one test surface.
- Adding the crate creates one lower-level dependency, but it does not become a
  shared domain-types package or move storage layout knowledge out of the
  owning Modules.
