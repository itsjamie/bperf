# Contributing to bperf

bperf makes performance decisions from browser evidence, so changes to capture,
measurement, or comparison code need a stronger check than ordinary unit
coverage. The fast suite should stay fast; the real-browser tests remain the
release gate.

## Set up the checkout

You need Rust with edition 2024 support and Node.js 24.12 or newer.

```text
npm --prefix sidecar ci
cargo build --locked
cargo run -- browsers install --engine all
```

Run the capability gate before working on an engine adapter:

```text
cargo run -- doctor --engine all
```

## Fast checks

These checks do not launch browsers:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
npm --prefix sidecar run check
```

Run focused tests while developing, then run the complete fast suite before
opening a pull request.

## Real-browser release gates

The ignored tests launch pinned Chromium, Firefox, and WebKit builds and write
large diagnostic artifacts:

```text
cargo test --test browser_contract firefox_doctor_does_not_spawn_node -- --ignored --exact
cargo test -p bperf-browser firefox::tests::browser_lab_uses_fresh_contexts_and_recovers_after_failure -- --ignored --exact
cargo test -p bperf-browser --test child_realms firefox_dedicated_workers_and_iframes_contribute_native_evidence -- --ignored --exact
cargo test --test browser_contract chromium_doctor_does_not_spawn_node -- --ignored --exact
cargo test -p bperf-browser chromium::tests::browser_lab_uses_fresh_contexts_and_recovers_after_failure -- --ignored --exact
cargo test -p bperf-browser --test child_realms chromium_dedicated_workers_and_iframes_contribute_native_evidence -- --ignored --exact
cargo test --test browser_contract webkit_doctor_does_not_spawn_node -- --ignored --exact
cargo test -p bperf-browser webkit::tests::browser_lab_uses_fresh_contexts_and_recovers_after_failure -- --ignored --exact
cargo test -p bperf-browser --test child_realms webkit_dedicated_workers_and_iframes_contribute_native_evidence -- --ignored --exact
cargo test every_engine_satisfies_the_capture_contract -- --ignored --exact
cargo test -p bperf-browser lab::tests::retained_lanes_keep_one_root_pid_and_shutdown_all_contained_processes -- --ignored --exact
cargo test --test measurement_contract variants_can_be_measured_and_compared_on_every_engine -- --ignored --exact
cargo test managed_benchmark_satisfies_every_engine_contract -- --ignored
```

The tests use `node` from `PATH` by default. Set `BPERF_NODE` to an absolute
executable path only when validating a different Node installation.

Run them with normal browser process permissions. Firefox uses its own content
process sandbox; do not disable it to make a test pass.

A capture change is not complete until every requested artifact succeeds on
all three engines. Do not add a shared path that silently supports only
Chromium or only the engines exposed through a public Playwright protocol
session. The child-realm gate must prove that named page, dedicated-worker, and
cross-origin iframe work appears in native evidence on every engine.

Run the direct Chromium, Firefox, and WebKit gates on every platform targeted
by the release. CI gives every engine its own fresh Ubuntu, Windows, and macOS
job, then runs the all-engine orchestration and comparison gates on Ubuntu.
Browser failures are reported directly; the workflow does not retry them or
disable an engine sandbox.

The package contract additionally builds the native target-triple archive,
installs only its executable into a clean Cargo root, downloads the pinned
browsers through that installed executable, runs `doctor --engine all`, and
measures the managed example. It runs on x86-64 Linux and Windows, plus Apple
Silicon and Intel macOS. This proves that release builds materialize their
embedded benchmark runtime instead of borrowing the checkout. The same job also
runs stock `cargo install --path`, provisions its source-embedded runtime, and
proves Chromium capture from that installation.

A `v*` tag must exactly match the Cargo package version. After the fast,
per-engine, cross-engine, and package contracts pass, CI publishes the four
archives and `SHA256SUMS` as a GitHub release. Do not create a release manually
from artifacts that skipped those gates.

`crates/bperf-browser/src/browser_process.rs` owns platform launch and
process-tree cleanup;
`crates/bperf-browser/src/artifacts.rs` owns artifact identity and the common Speedscope
viewer schema;
`crates/bperf-browser/src/chromium.rs`, `firefox.rs`, `firefox_rdp.rs`, and
`webkit.rs`
own their engine protocols and native capture formats.
`crates/bperf-runtime` owns release-runtime embedding, atomic materialization,
Playwright registry discovery, and pinned browser installation.
`sidecar/src/benchmark-host.ts` may bundle and serve benchmarks but must not
launch a browser.

The TypeScript runtime contains no browser adapter or capture transport. The
retained-PID gate must show one healthy root per engine, and successful shutdown
must prove that its process group or Job Object contains no active process.

## Design changes

Start with [docs/DESIGN.md](docs/DESIGN.md) and the
[ADR index](docs/adr/README.md).

For a major public interface, compare at least two designs before choosing one.
Prefer a small interface that hides browser protocols, generated state,
sampling, and persistence. Domain concepts should cross module boundaries;
CDP, RDP, Gecko Profiler, Web Inspector, and Playwright-private objects should
not.

Add an ADR when the reason for a decision will matter after the implementation
is no longer new. An ADR should include the pressure that forced the decision,
the alternatives that were seriously considered, the chosen contract, and the
costs that remain.

## Documentation

Lead with the outcome or the problem the reader is trying to solve. Explain why
a constraint exists before listing its mechanics.

Use concrete names, commands, and evidence. Separate what is implemented from
what is proposed. If a result has an important caveat, put it beside the result
instead of hiding it in a later section.

Keep these document boundaries:

- `README.md` introduces bperf and the shortest complete path.
- `docs/AUTHORING.md` owns the benchmark authoring contract.
- `docs/OPTIMIZATION.md` owns the baseline and candidate workflow.
- `docs/DESIGN.md` owns system invariants and module boundaries.
- `docs/adr/` records why durable design decisions were made.
- Prototype notes describe historical experiments, not supported interfaces.

Comments in code should explain intent, invariants, constraints, or surprising
behavior. Do not restate the next line or preserve development history in a
comment.

## Pull requests

Keep a pull request focused enough that its design and evidence can be reviewed
together. Include:

- what changed;
- why the change is needed;
- the part of the contract a reviewer should examine closely;
- fast and real-browser tests that were run;
- benchmark evidence when the change claims a performance improvement.

Performance results must remain separated by engine. Include baseline and
candidate values beside percentage effects, and call out inconclusive anchors
or protected regressions directly.
