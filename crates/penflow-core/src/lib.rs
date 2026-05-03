//! Penflow capture + encode + inject engine.
//!
//! See `docs/design.md` §6 for the architecture and `docs/HANDOFF.md` §3.3 /
//! §4.4b / §4.5 for the gate-2 findings that shape the Windows hot-path.

pub mod error;

#[cfg(windows)]
pub mod d3d11;
#[cfg(windows)]
pub mod monitors;
#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod color;
#[cfg(windows)]
pub mod encoder;

pub mod packet_queue;
#[cfg(windows)]
pub mod pipeline;

pub use error::{EngineError, EngineResult};

/// Returns a build identifier string. Kept as a cross-crate sanity test
/// until the real `Engine` API lands (§5.2 step 11).
pub fn build_id() -> &'static str {
    "penflow-core v0.1.0 (engine-foundations)"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_id_is_present() {
        assert!(build_id().starts_with("penflow-core"));
    }
}
