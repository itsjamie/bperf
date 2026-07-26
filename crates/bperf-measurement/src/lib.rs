//! Immutable benchmark definitions, schedules, samples, and artifact retention.

pub const MEASUREMENT_SCHEMA_VERSION: u32 = 4;

pub mod manifest;
pub mod retention;
pub mod sampling;
pub mod schedule;
pub mod store;
