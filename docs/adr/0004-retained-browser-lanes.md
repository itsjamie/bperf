# ADR 0004: One retained browser lane per engine

Status: accepted · 2026-07-25

## Context

A browser launch is orchestration cost, not benchmark evidence. Launching a
fresh process for every sample made short browser workloads spend most of their
runtime in startup. Direct measurement showed that 64 hls.js parser invocations
took 2-4 ms while one fresh-process Firefox capture took about 4.7 seconds.

Three lifecycle designs were considered.

1. Launch a browser for every trial.
2. Retain one browser and page across all trials for an engine.
3. Retain one browser process for each engine, while creating a fresh browser
   context and page for every trial.

The first repeats expensive work outside every reported metric. The second
allows cookies, storage, workers, page globals, and live objects to cross sample
boundaries.

## Decision

Each measurement set uses one retained combined-trial lane for each of
Chromium, Firefox, and WebKit. A lane launches lazily and remains alive until
the sidecar shuts down. Every trial receives a new browser context and page;
the context is closed before the next trial enters that lane.

Lanes execute trials serially. A browser or protocol failure invalidates the
current attempt and closes that lane. A later attempt must create a new lane
rather than continuing with uncertain engine state. Baseline, candidate, and
confirmation measurement sets never share browser processes.

The benchmark adapter remains alive across trials so every retained browser
loads the same prepared variant. Adapter state is not exposed to benchmark
authors.

## Consequences

- Browser launches scale with active engines instead of sample count.
- Cookies, storage, cache, service workers, and page globals cannot cross
  sample boundaries.
- Process-level warm state is standardized by calibration rather than reset for
  every observation. The common path warms during pilot sizing; the advanced
  protocol may declare complete warmup trials.
- Browser PID is stable within a lane but is not variant identity.
- One retained process does not change the per-trial isolation or
  combined-capture contract; see ADR 0005.

## Validation

The hls.js fixture ran eight fixed-batch observations under retained and
fresh-process lifecycles after three retained-lane warmups. Every retained lane
used one stable browser PID. Fresh-process observations used a new PID for
every pass.

Median orchestration time fell by 35% on Chromium and 75% on Firefox. WebKit
median time changed by 4%, with substantially lower elapsed-time variance.
Live heap medians were identical on Chromium and Firefox and differed by less
than 0.01% on WebKit.

Retained Firefox timing and CPU medians were 14% and 12% lower than cold-process
medians. This is an intentional change from cold-process to standardized
warmed-process measurement. Browser-lab protocol version 6 introduced retained
lanes. Protocol version 8 reduces the three per-engine lanes to one combined
lane. Environment identity prevents either model from being compared with
evidence collected under a previous lifecycle.
