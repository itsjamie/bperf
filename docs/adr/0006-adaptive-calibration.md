# ADR 0006: Adaptive calibration prefixes

Status: accepted · 2026-07-25

## Context

The standard managed benchmark originally ran three complete warmup trials and
ten complete pilot trials for every workload and engine. One hls.js workload
therefore needed 39 calibration trials before its 60 final trials. The pilot
itself already performs unprofiled batch-sizing probes before starting CPU
capture, so the separate warmup captures repeated expensive CPU, flamegraph, and
heap work without contributing to the sampling decision.

Three calibration models were considered.

1. Keep three complete warmups and ten pilots for every stratum.
2. Add a second browser protocol for lightweight warmups, then keep ten pilots.
3. Let pilot sizing provide warm-up and stop each pilot stratum when its
   decision estimates stabilize.

The first ignores evidence that calibration has already converged. The second
adds an engine-facing lifecycle solely to repeat work that the sizing probes
already perform.

## Decision

The standard managed path schedules no separate warmup trials. The advanced
manifest may still request them, and any requested warmup remains a complete
trial under the existing capture contract.

The pilot count is a maximum schedule envelope rather than an exact count. The
standard policy checks each benchmark case and engine after five pilots and
allows at most ten. A stratum is stable when all of these estimates remain
within their tolerances across the latest three cumulative prefixes:

- every primary metric's required final-sample count: 10%, with a minimum
  tolerance of two samples;
- median selected batch size: 20%;
- median complete-trial elapsed time: 20%.

Stable strata stop independently. An unstable stratum continues one
deterministic pilot at a time until it stabilizes or reaches the cap. The final
decision records each selected pilot prefix and whether it stopped through
stability or the maximum.

The immutable schedule remains a maximum envelope. Before `sampling.json`
exists, resume recomputes the next pilot from append-only evidence. Once all
strata stop, `sampling.json` locks both pilot and final prefixes. Opening that
measurement again verifies the recorded stop reason against the selected pilot
evidence and rejects missing or extra pilot suffixes.

Measurement schema version 4 distinguishes this scheduling meaning from the
fixed calibration behavior. The combined browser capture contract is unchanged,
so browser-lab protocol version 8 remains current.

## Consequences

- Stable engines avoid unnecessary complete calibration captures.
- A noisy engine may continue without forcing additional pilots on other
  engines.
- Final trial counts and all final capture guarantees are unchanged.
- Calibration duration remains evidence-based and may still reach ten pilots
  per stratum.
- Explicit advanced warmups remain available without complicating the common
  authoring interface.

## Validation

Unit coverage proves that a stable stratum stops at five pilots, a capped noisy
stratum records the maximum-samples reason, and stable Chromium and WebKit
strata remain stopped while noisy Firefox advances alone. Resume coverage
proves that only the locked pilot and final prefixes remain active.

The real hls.js MP4 benchmark stopped all three engines at five pilots. It
completed 15 calibration trials and the same 60 final trials in 51.2 seconds,
down from 39 calibration trials and 75.7 seconds under the previous policy.
Recorded calibration trial time fell from 22.5 to 10.1 seconds. The final
evidence still contains 20 complete samples for each engine.
