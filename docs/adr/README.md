# Architecture decisions

The design document describes the current system. These records explain the
decisions whose alternatives and tradeoffs still matter.

| ADR | Decision |
|---|---|
| [0001](0001-runtime-validity.md) | Check historical comparisons with per-engine runtime anchors and require independent confirmation after repeated candidate search. |
| [0002](0002-artifact-retention.md) | Keep complete statistical evidence while retaining only representative native diagnostic payloads. |
| [0003](0003-browser-project-bundles.md) | Bundle the project's benchmark entry instead of reproducing package resolution in the fixture server. |
| [0004](0004-retained-browser-lanes.md) | Retain one browser process per engine and isolate each trial with a new context and page. |
| [0005](0005-combined-final-trials.md) | Capture wall, CPU, flamegraph, and heap evidence around one workload execution per trial. |
| [0006](0006-adaptive-calibration.md) | Use pilot sizing for warm-up and stop each case/engine pilot prefix independently. |
| [0007](0007-rust-browser-adapters.md) | Move all browser capture into Rust and organize runtime, browser, measurement, and decision knowledge behind explicit crate Interfaces. |

Accepted records are not edited to make an old decision look current. If the
decision changes, add a new ADR that supersedes the old one and update
[docs/DESIGN.md](../DESIGN.md).
