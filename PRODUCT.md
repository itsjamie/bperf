# bperf product

## Register

product

## Users

Performance engineers and coding agents use bperf from a terminal while
iterating on deterministic browser code. They need to understand an
optimization lineage quickly, compare evidence within Chromium, Firefox, and
WebKit, inspect representative artifacts, and decide what to measure or promote
next.

## Product Purpose

bperf turns browser-performance evidence into trustworthy optimization
decisions. The history interface makes the latest benchmark state, measured
cycles, per-engine outcomes, retained evidence, and promotion readiness
available in one keyboard-driven view without weakening the underlying
three-engine comparison contract.

## Brand Personality

Precise, terse, and evidence-first. The interface should feel calm under dense
technical information, use domain language consistently, and reward expert
users without hiding essential context.

## Anti-references

Do not imitate a conventional web analytics dashboard, card-based SaaS
interface, or decorative retro terminal. Avoid ornamental chrome, oversized
metrics, unexplained iconography, color-only meaning, and layouts that pool or
visually subordinate one browser engine.

## Design Principles

- Keep the decision and its evidence visible together.
- Give Chromium, Firefox, and WebKit equal visual and interaction weight.
- Make the common review path fast while keeping deeper artifacts one key away.
- Use density to support comparison, not to expose storage or protocol details.
- Preserve useful behavior in pipes, automation, and constrained terminals.

## Accessibility & Inclusion

The interface is keyboard-operated and does not rely on color alone for
verdicts or selection. Text labels and stable symbols accompany semantic color.
No additional accessibility standard or user-specific accommodation is
required for the initial TUI.
