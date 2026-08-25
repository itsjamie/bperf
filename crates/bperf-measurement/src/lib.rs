//! Immutable benchmark definitions, schedules, samples, and artifact retention.

/// Bumped whenever a recorded metric changes meaning, so older baselines are
/// reported as incomparable instead of compared. Schema 8 drops the inline
/// source map from the benchmark bundle: `browser.js_heap.live_bytes` no
/// longer includes the map's data URL string, which grew with the bundled
/// source text and read as a live-heap regression on every code change.
pub const MEASUREMENT_SCHEMA_VERSION: u32 = 8;

pub mod manifest;
pub mod retention;
pub mod sampling;
pub mod schedule;
pub mod store;
