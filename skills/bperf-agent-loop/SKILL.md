---
name: bperf-agent-loop
description: Runs browser performance optimization loops with bperf while preserving baselines, source history, and engine-specific timing, CPU, flamegraph, and heap evidence. Use when an agent needs to benchmark browser code, establish or promote a bperf baseline, iterate on a performance hypothesis, inspect bperf history or profiles, or decide whether a browser-code candidate improved.
---

# bperf agent loop

## Preconditions

1. Read the repository's `AGENTS.md` and benchmark source.
2. Run `bperf --version` and require Node 24.12 or newer.
3. Locate the relevant `*.bench.ts`. If none exists, use the repository's
   bperf authoring guide when present, then the minimal shape in
   [REFERENCE.md](REFERENCE.md).
4. Run `bperf doctor --engine all` once for a new host or browser installation.
   Chromium, Firefox, and WebKit are one contract; never weaken the benchmark to
   make one engine pass.

On Windows, Firefox may fail inside an agent command sandbox with
`Failed to launch tab subprocess @SB::LA::SpawnTarget (Error:0)`, followed by
the misleading `browserContext.newPage` error about an undefined `_page`.
Rerun the exact bperf command outside the command sandbox. Treat it as a browser
or adapter defect only if the unrestricted retry also fails.

## Establish the baseline

Do not repeatedly measure an unchanged baseline.

```text
bperf run <benchmark.ts> --budget <duration> --message "Establish <subject> baseline"
bperf accept <cycle-id>
```

Accept the initial cycle only after correctness succeeds on every engine. The
promoted immutable measurement becomes the comparison baseline for later runs.

## Optimize

Repeat this loop:

1. Read `bperf history <benchmark-id> --format agent-context`.
2. Form one concrete performance hypothesis.
3. Change only the benchmark subject. Run focused correctness tests.
4. Measure once:

   ```text
   bperf run <benchmark.ts> --budget <duration> --message "<hypothesis and change>"
   ```

5. Inspect the compact stdout summary: overall verdict, every engine's
   correctness and anchor, and every primary metric's classification, effect,
   baseline-to-candidate values, and confidence interval. Never pool or average
   measurements across browsers.
6. Use `bperf show <cycle-id> --diff` to verify the recorded source change.
7. For a positive candidate, inspect representative CPU/flamegraph and heap
   evidence where it explains the result. Accept only when the per-engine
   tradeoff matches the optimization goal.
8. For a negative or inconclusive candidate, keep the recorded cycle, learn
   from it, and make the next hypothesis. Treat an equivalent candidate the
   same way unless it is an intentional tradeoff with another concrete benefit.
   Do not promote a result merely because one engine improved.

If `accept` requests independent evidence:

```text
bperf confirm <cycle-id> <benchmark.ts> --budget <duration>
bperf accept <cycle-id>
```

Do not edit the source between the candidate and its confirmation.

## Commit an accepted change

When the user has requested a commit, create it only after `bperf accept`
succeeds:

1. Compare `git diff` with `bperf show <cycle-id> --diff`. Stage the measured
   subject change and its focused tests. Never create a metrics-only commit,
   stage unrelated edits, or commit `.bperf/`.
2. Recheck `git diff --cached` so the accepted implementation is actually in
   the commit.
3. Use `perf(<scope>): <imperative summary>` for a scoped change or
   `perf: <imperative summary>` when no scope improves the title. Keep the
   subject under 72 characters and omit the trailing period.
4. Start the body after a blank line. Explain what changed, why it removes
   work, and which behavior benefits. Wrap prose at 72 columns.
5. Add one blank-line-separated block per engine. Put the engine on its own
   heading line, then one complete metric per line. Keep each metric's label,
   effect, and `(baseline -> candidate)` values on the same physical line; wrap
   only between metrics, even when a metric line slightly exceeds 72 columns.
   Label positive percentages as improvements and label the anchor separately
   as drift. Never pool browsers or leave a bare `+N%` ambiguous.
6. Use the confirmation metrics when independent confirmation was required;
   otherwise use the accepted candidate metrics. Add focused tests and
   `Bperf-Benchmark` / `Bperf-Cycle` trailers for traceability.

Use the [accepted-change commit template](REFERENCE.md#accepted-change-commit-template).
Do not add the four-space outer indentation shown by `git log`; the pager adds
that. Use two spaces only to group metric rows under an engine heading.

## Evidence rules

- Use the default `run` and `confirm` output for decisions. `--json` emits the
  complete machine-readable document and is for programmatic consumers, not the
  normal agent loop.
- The summary links to `summary.json`, `sampling.json`,
  `artifact-retention.json`, and, when a baseline exists, `comparison.json`.
  Resolve only the evidence needed for the current decision; do not dump a
  complete JSON file into context when selecting a few fields is sufficient.
- Read `comparison.json` when a result is negative or inconclusive, an engine
  disagrees with the overall direction, correctness or an anchor does not pass,
  a guardrail regresses, a warning is present, or an interval is close enough
  to a policy threshold that the summary does not settle the decision.
- Read `sampling.json` and the measurement `summary.json` when evidence is
  incomplete, budget-limited, insufficient, unexpectedly noisy, or when trial
  counts and pilot stopping behavior affect the next step.
- Read `artifact-retention.json`, then only the selected CPU/flamegraph or heap
  artifacts it names, when a representative profile could explain a result,
  distinguish competing hypotheses, or validate an important engine-specific
  tradeoff before promotion. Do not open profiles merely to restate a clear
  statistical verdict.
- Use `bperf show <cycle-id> --diff` before promotion. Add `--json` only when a
  structured field not present in the compact output is required, and select
  that field rather than returning the whole document.
- Exit `0`: measured baseline, positive, or equivalent; inspect the summary
  verdict before promotion.
- Exit `1`: negative.
- Exit `2`: incomplete or inconclusive. Correct the environment or resume the
  exact command; do not interpret it as no regression.
- The budget is an approximate measurement target. The minimum statistical
  evidence floor can require more time.
- Rerunning the exact command resumes compatible pending evidence.
- Benchmark or fixture changes redefine the benchmark identity. Establish a new
  baseline instead of comparing across that change.
- Do not delete or edit `.bperf/` during an optimization loop. bperf compacts
  completed measurements while retaining history and representative profiles.

## Finish

The loop is complete when a candidate is correct on all engines, has acceptable
engine-specific evidence, passes confirmation when required, and is promoted
with `bperf accept`. Report the cycle ID, baseline transition, per-engine
verdicts, important metric intervals and baseline-to-candidate values, and
focused correctness tests. Keep the raw values beside each effect percentage.
A bare `+N%` can be mistaken for increased resource usage.
