# ADR 0005: Combined final trials

Status: accepted · 2026-07-25

## Context

Wall timing, sampled CPU activity, and live heap size have different variance
and capture costs. Scheduling them as independent final streams avoids some
captures when one metric needs more observations, but it changes the unit the
user requested. A fixed count of 20 becomes 20 timing executions, 20 CPU
executions, and 20 heap executions per engine. Across Chromium, Firefox, and
WebKit that is 180 workload executions rather than 60.

Three scheduling models were considered.

1. Keep independent timing, CPU, and heap final streams.
2. Run one workload execution per trial and collect all evidence around it.
3. Share one workload execution for timing and CPU, but capture heap in a
   separate execution.

The first and third models lose the relationship between the observed wall,
CPU, and heap values. They also require users to translate a requested sample
count into multiple hidden workload executions.

## Decision

One trial produces one complete sample. After setup, the engine adapter starts
its native CPU profiler, runs and times one calibrated workload batch, stops the
profiler, settles the same page, and captures its live JavaScript heap. The
trial returns:

- normalized workload and variant-call wall time;
- normalized target-attributed CPU activity;
- live JavaScript heap size after the batch;
- the native CPU profile;
- its Speedscope flamegraph;
- the native heap snapshot.

Wall timing is profiler-instrumented by design. Browser startup, setup,
settling, and heap-capture time remain outside `workload.wall_ms`.

Every scheduled warmup, pilot, and final phase uses the same capture contract.
The common managed path schedules no separate warmup trials because pilot sizing
probes already warm the subject. Explicit advanced warmups and pilots may run
unmeasured sizing probes before their captured batch. Locked final trials run
exactly one batch and do not recalibrate.

Adaptive sampling estimates the requirement for every primary metric, then
uses the largest requirement as the one final-trial count for that benchmark
case and engine. A fixed count of `N` means `N` complete final trials, each
with all three native artifacts. Trial identifiers do not contain a capture
kind.

Measurement schema version 3 introduced this model, and schema version 4 retains
it while adding adaptive pilot prefixes. Browser-lab protocol version 8
prevents combined-trial evidence from sharing identity with independent-stream
evidence.

Every trial still executes correctness verification. Comparison remains
per-case and per-engine; it never pools values across browsers.

## Consequences

- Twenty final samples on three engines means 60 workload executions, 60 CPU
  profiles, 60 flamegraphs, and 60 heap snapshots.
- Wall, CPU, and heap values retain their trial-level relationship.
- The noisiest primary metric controls the number of complete captures.
- Some artifacts may be collected beyond the precision requirement of their
  own metric.
- The runtime budget is allocated using complete-trial cost.
- Artifact retention may select CPU and heap representatives from different
  trials because their medians can occur in different samples.
- Calibration evidence is not used as final performance evidence.

## Validation

The scheduling tests give CPU pilots higher variance than wall and heap
metrics. The complete trial count grows to the CPU requirement, while every
selected trial retains all metrics and all three native artifacts. A fixed
20-sample schedule contains 20 final trials per case and engine.

The first real hls.js MP4 benchmark completed 39 fixed calibration trials and
60 final trials in 75.7 seconds on the development machine. Its final records
contain 20 complete samples for each engine: 60 CPU profiles, 60 flamegraphs,
and 60 heap snapshots in total. ADR 0006 records the later adaptive-calibration
result.
