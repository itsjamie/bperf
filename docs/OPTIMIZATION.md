# Running an optimization

bperf keeps the measurement evidence separate from the decision to promote it.
That distinction matters: a completed candidate is not automatically a better
baseline.

## Prove the machine first

Run the complete capability gate on a new host or after changing the browser
installation:

```text
bperf doctor --engine all
```

Chromium, Firefox, and WebKit must all pass. If one engine cannot produce a
requested artifact, fix the environment before measuring a candidate.

On Windows, Firefox can fail when launched inside a restrictive command
sandbox. A typical failure includes
`Failed to launch tab subprocess @SB::LA::SpawnTarget`, followed by a misleading
Playwright error about an undefined page. Retry the same command with normal
browser process permissions. Do not disable Firefox's content sandbox.

## Establish the baseline

Measure the first source state once:

```text
bperf run benchmarks/parser.bench.ts --budget 5m --message "Establish parser baseline"
```

With no promoted baseline, the outcome is `measured`. Check that correctness
passed in every engine, then promote the cycle:

```text
bperf accept <cycle-id>
```

Promotion appends a reference to the immutable measurement set. It does not
rename, merge, or rewrite the evidence.

## Measure one hypothesis

Make one source change whose expected effect can be stated plainly, run focused
correctness tests, and measure it:

```text
bperf run benchmarks/parser.bench.ts --budget 5m --message "Reuse parsed box metadata"
```

The message is stored with the source checkpoint and comparison. It should say
what changed or what work should disappear, not merely "optimize parser."

Before promoting the result, verify the recorded source boundary:

```text
bperf show <cycle-id> --diff
```

The main thing to avoid is selecting the one flattering browser or metric.
bperf's overall verdict is strict because browser engines can disagree in ways
that matter to users.

## Read the verdict

| Verdict | What happened |
|---|---|
| `positive` | Correctness passed and every required engine met the improvement policy. |
| `equivalent` | The candidate stayed within the accepted effect and guardrail thresholds. |
| `negative` | Correctness failed or at least one required engine regressed. |
| `inconclusive` | The sample, anchor, or compatibility evidence cannot support a decision. |

Every effect should be read with its baseline and candidate values:

```text
effect=+52.45% (100ms -> 47.55ms)
```

The positive percentage means improvement: the measured cost fell. The values
are workload-weighted geometric point estimates, so they remain consistent
with the percentage when a benchmark has several cases.

Do not compare absolute CPU, wall, or heap values between engines. Native
profilers do not have identical semantics, and bperf makes no claim that they
do. Compare a candidate with its baseline within the same engine.

## Open deeper evidence when it changes the decision

The compact output links to the persisted evidence. Start there, then open only
what answers the current question.

Read `comparison.json` when:

- the result is negative or inconclusive;
- browsers disagree;
- correctness or a runtime anchor does not pass;
- a protected metric regresses;
- a confidence interval sits near a policy threshold;
- the summary reports a warning.

Read `sampling.json` and the candidate's `summary.json` when:

- measurement is incomplete or budget-limited;
- a case was unexpectedly noisy;
- pilot stopping or final trial counts affect the next step.

Read `artifact-retention.json` before opening profiles. It identifies the final
trial nearest the median CPU metric and the final trial nearest the median heap
metric for each case and engine. Those may be different trials because they
represent different distributions.

Open a retained CPU profile, Speedscope flamegraph, or heap snapshot when it can
explain the result or distinguish the next hypotheses. A profile is useful
diagnostic evidence; it does not replace the statistical comparison.

## Runtime anchors

A historical baseline saves time, but identical version strings cannot prove
that the host is performing the same way. Thermal state, virtualization, and
background load can move engine results without changing an environment
fingerprint.

Every measurement records a small versioned JavaScript CPU anchor in each
engine. Comparison classifies the historical and fresh anchor distributions
independently:

- `stable`: the 95% drift interval is wholly inside ±5%;
- `drifted`: the interval is wholly outside ±5%;
- `inconclusive`: the interval crosses the boundary;
- `unproven`: one side has no current anchor evidence.

A non-stable anchor makes performance evidence inconclusive for that engine.
It does not excuse a correctness failure.

Exact host, per-engine browser, and per-engine adapter identity is a separate
compatibility gate. Every engine identifies its Rust adapter protocol, pinned
Playwright revision, and executable digest. Baseline age is reported and warns
after seven days, but age alone does not override a stable fresh anchor.
Measurements created by former Node-owned browser adapters must be remeasured.

[ADR 0001](adr/0001-runtime-validity.md) records why bperf uses anchors and
independent confirmation instead of reconstructing old source inside the
caller's worktree.

## Accept or continue

Promote a candidate only when its engine-specific tradeoff is acceptable:

```text
bperf accept <cycle-id>
```

Negative, equivalent, inconclusive, and reverted cycles remain in history. They
are useful because they show which source changes were actually measured and
what happened.

Continue with a new hypothesis when the candidate is not worth promoting. Do
not delete the failed cycle or overwrite its evidence.

## Independent confirmation

Repeatedly testing candidates against one baseline introduces selection risk.
After five candidate cycles use the same baseline, `accept` requires a new
measurement of the unchanged candidate:

```text
bperf confirm <cycle-id> benchmarks/parser.bench.ts --budget 5m
bperf accept <cycle-id>
```

Do not edit the benchmark or source between the original candidate and its
confirmation.

Confirmation has its own resumable measurement identity and is appended to the
lineage. It is not recorded as another source-change cycle.

## Resume an interrupted run

Repeat the exact `run` or `confirm` command. bperf reopens the compatible
measurement schedule, keeps valid trials, and retries only missing or invalid
attempts.

Do not edit or delete `.bperf/` during an optimization loop. Benchmark, source,
or fixture changes can produce a different identity instead of resuming the
pending measurement.

An inconclusive runtime anchor is not repaired by increasing the benchmark
budget. Fix or stabilize the environment first.

## Inspect history

```text
bperf history <benchmark-id>
bperf history <benchmark-id> --format agent-context
bperf show <cycle-id> --diff
```

History is independent of Git. It includes uncommitted and untracked project
modules that were present in the measured graph. It does not record edits until
`run` completes, and it does not claim to capture private reasoning between
measurements.

Returning to an older source state after another cycle creates a visible
reversion cycle. An exact retry of the latest source and evidence is
idempotent.

## Machine-readable output and exit codes

Use `--json` when another program needs the complete result. The default output
is intended for decisions and links to the larger JSON documents.

| Exit code | Meaning |
|---:|---|
| 0 | Baseline-free measurement completed, or comparison is positive/equivalent. |
| 1 | Comparison is negative. |
| 2 | Measurement or comparison is incomplete/inconclusive. |

Always read the outcome as well as the exit code. Exit 0 for a baseline-free
run means only that evidence was collected.
