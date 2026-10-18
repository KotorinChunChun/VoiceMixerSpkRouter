//! Device-independent signal generation and analysis for automated tests.
//!
//! Compiled only for `cargo test` and crates that enable the `testing` feature
//! so it is not linked into the product binary.

pub mod analyzer;
pub mod edge_client;
pub mod signal_gen;
pub mod writer;

pub use analyzer::{
    envelope_ms, find_clicks, max_sample_step, peak_dbfs, rms_dbfs, snr_db, tone_level_dbfs,
};
pub use signal_gen::{generate, SignalKind, SignalSpec};
pub use writer::{write_audio, AudioFileFormat};
