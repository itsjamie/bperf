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
| [0008](0008-child-execution-realms.md) | Include dedicated workers and iframes through engine-native capture scopes without changing benchmark statistics. |
| [0009](0009-release-distribution.md) | Embed the locked benchmark runtime in each native executable and publish only after installed-package contracts pass. |
| [0010](0010-rust-project-bundler.md) | Bundle managed benchmark projects with Rolldown inside Rust and make the materialized bundle part of variant identity. |
| [0011](0011-rust-benchmark-host.md) | Serve materialized bundles and locked fixtures from Rust, leaving Node outside browser serving and trial execution. |
| [0012](0012-rust-fixture-acquisition.md) | Acquire, cache, and lock local and remote fixtures in Rust so managed runs and confirmations never start Node. |
| [0013](0013-rust-browser-distribution.md) | Generate an authenticated static Playwright registry and install browser archives entirely in Rust, removing Node.js from bperf. |
| [0014](0014-crash-safe-local-persistence.md) | Publish local files atomically and make append-only histories recoverable without moving domain schemas into shared storage code. |
| [0015](0015-canonical-sqlite-storage.md) | Use one canonical SQLite database for structured state while retaining large native payloads as validated files. |

Accepted records are not edited to make an old decision look current. If the
decision changes, add a new ADR that supersedes the old one and update
[docs/DESIGN.md](../DESIGN.md).
