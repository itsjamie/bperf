# ADR 0001: Runtime anchors and promotion confirmation

Status: accepted · 2026-07-24

## Context

Historical baselines avoid repeating an expensive browser measurement for every
source edit, but identical browser versions and configuration do not prove that
the host is performing consistently. Thermal state, virtualization, background
load, and operating-system behavior can move timing and sampled-CPU results
without changing the environment identity.

Repeatedly searching candidates against one historical baseline also introduces
selection risk: the apparent winner may partly reflect measurement noise.

Two designs were considered.

1. Reconstruct the promoted source state in a temporary workspace and rerun a
   subset of its benchmark cases.
2. Record a small, versioned browser-runtime workload with every measurement
   set and compare its historical and fresh distributions.

The first design looks workload-specific, but the discovered module graph is
not a complete project checkout. Builds may need package-manager state,
generated files, transforms, aliases, or files that the measured path did not
load. Reconstructing those inputs would either fail for valid projects or
silently measure different code. Mutating the caller's worktree to obtain the
old state is not acceptable.

## Decision

Every engine records 31 calibrated observations of `javascript_cpu_v1` during
preflight, after four warmups and outside profiler capture. Each observation
batches enough repetitions to target 75 milliseconds and is normalized to one
anchor unit. The environment fingerprint contains exact Node, Playwright,
operating-system, CPU, and browser-build identity; performance observations do
not participate in that identity.

Comparison independently bootstraps the median-duration change for Chromium,
Firefox, and WebKit. The anchor is stable only when its 95% interval is wholly
inside ±5%. An interval wholly outside that band is drifted. An interval that
crosses the boundary is inconclusive. Drifted or inconclusive anchor evidence
makes performance results inconclusive for that engine, while correctness
failures remain failures.

The environment record stores its capture time. Comparisons report baseline age
and warn after seven days, but age alone does not override stable fresh anchor
evidence.

After five measured candidates have used one baseline, promotion requires an
independent confirmation measurement. `bperf confirm` uses a deterministic
confirmation cohort, so it creates a distinct resumable measurement set without
pretending that the unchanged source is another optimization cycle. The
confirmation remains an append-only lineage event linked to the selected cycle.

## Consequences

- bperf detects broad browser/host performance drift without Git access,
  worktree mutation, or project reconstruction.
- Browser revisions remain an exact compatibility gate, separate from
  statistical drift.
- Anchor results are never pooled across engines or merged into workload
  samples.
- The anchor is intentionally generic. It cannot prove that every
  workload-specific subsystem is drift-free; it provides a conservative
  runtime validity signal.
- Confirmation adds cost only after repeated search and is resumable and
  idempotent.
