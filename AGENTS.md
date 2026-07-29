# Agent instructions

## Software-design philosophy

Use the principles from John Ousterhout's *A Philosophy of Software Design* when
designing and reviewing this repository.

- Prefer deep modules: a small, stable interface should hide substantial
  implementation complexity.
- Assign each design decision to exactly one module. Treat duplicated knowledge
  and shared hidden assumptions as information leakage.
- Organize modules around the knowledge they own, not the order in which work
  happens. Avoid temporal decomposition.
- Avoid classitis and one-file-per-trivial-concept decomposition. A module must
  hide meaningful complexity to justify its interface.
- Make interfaces somewhat general-purpose when that simplifies the common case,
  but do not add speculative abstractions.
- Define errors out of existence where practical. Otherwise, report them at the
  layer with enough context to make them actionable.
- Different layers must provide different abstractions. Pass domain concepts
  across boundaries, not lower-layer protocol details.
- Keep the common path simple and configuration-light. Hide defaults and
  bookkeeping inside the module that owns them.
- Comments should document abstractions, invariants, design intent, and
  non-obvious reasoning. Do not merely restate code.
- Consider at least two designs before committing a major public interface.
- During review, treat complexity reduction as a required feature. Flag shallow
  modules, information leakage, change amplification, excessive cognitive load,
  and unknown-unknowns.

### Comments

- Write comments only for information the code cannot express clearly: intent,
  invariants, constraints, surprising behavior, and the reason behind a design
  decision.
- Interface comments define the abstraction from the caller's perspective:
  guarantees, required preconditions, important error behavior, and what remains
  hidden. Do not describe the interface as "deep" or narrate the design process.
- Implementation comments explain why a non-obvious technique is necessary.
  Do not restate names, signatures, types, control flow, or the next line of code.
- Avoid meta-commentary such as "this module owns," "everything below is
  private," "we intentionally," or "this keeps callers from." State the actual
  invariant or constraint directly.
- Do not preserve development history in code comments. Put durable architectural
  decisions and rejected alternatives in an ADR when that history matters.
- If the same explanatory comment is needed in several modules, treat that as
  possible information leakage and look for a single authoritative location.
- Prefer precise domain language over generic software-design terminology.
- Delete or rewrite comments that become redundant after the code is clarified.
  An inaccurate comment is worse than no comment.

## Benchmark domain language

- A **benchmark subject** is the behavior or code being evaluated. Do not call it
  a tool merely because an executable adapter exposes it.
- A **variant** is one concrete implementation of that subject.
- A **workload** is the deterministic operations and inputs applied to both
  variants.
- A **benchmark case** fixes the subject, workload, engine, and environment
  configuration being compared.
- A **trial** runs one variant for a benchmark case. It produces one **sample**
  containing wall timing, CPU, flamegraph, and heap evidence from the same
  workload execution. A **measurement set** contains trials for one variant.
- A trial's **phase** is warmup, pilot, or final. Every scheduled phase uses the
  same capture contract. The common managed path has no separate warmup trials;
  the pilot's sizing probes warm the retained browser lane before capture.
- An adaptive pilot schedule is a maximum envelope. Each case and engine stops
  independently at a deterministic prefix, and `sampling.json` locks that
  prefix before final evidence begins.
- A fixed count of `N` means `N` complete final trials for each benchmark case
  and engine.
- **Baseline** and **candidate** are comparison roles for immutable measurement
  sets, not properties embedded in a benchmark or variant definition.
- A **benchmark adapter** is invocation and browser-lease glue. Keep adapter and
  transport details out of the subject, workload, and statistics schemas.

## Non-negotiable browser contract

- Chromium, Firefox, and WebKit are first-class engines.
- Do not introduce a shared abstraction that silently supports only one or two
  engines.
- Requested captures must either succeed on every requested engine or fail
  preflight explicitly. Never silently downgrade or omit an artifact.
- Keep engine-protocol knowledge inside its adapter. Core modules must not expose
  CDP, Firefox RDP, Gecko Profiler, Web Inspector, or Playwright private objects.
- Compare results within an engine. Do not pool raw measurements across engines
  by default.

## Runtime distribution

- Node.js and npm are not build, test, packaging, installation, or runtime
  dependencies.
- Browser-side TypeScript is bundled with Rolldown inside Rust.
- Browser builds and Linux packages are installed by `bperf-runtime`; do not
  reintroduce a package-manager CLI or external browser automation process.
- `crates/bperf-runtime/playwright-registry.json` is generated from an
  authenticated `playwright-core` package. Do not edit distribution paths or
  dependency lists by hand; update it through the `bperf-build`
  `playwright-registry` command.
