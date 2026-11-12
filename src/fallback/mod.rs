//! dsync#129 slice 2 — the dsync fallback time source.
//!
//! On a network with NO PTP leader at all (no PTPv1 or PTPv2 Sync / Follow_Up from any source for
//! `PTP_TIMEOUT_SECS`), one dsync node is elected by its configured priority and multicasts its
//! time line on a dsync-PRIVATE group (`wire::GROUP`:`wire::PORT`, never 224.0.1.129 and never
//! ports 319/320), and every other dsync node follows it through the same rate servo and phase
//! lock as PTP. The first PTP time message seen moves every source and follower back to
//! `candidate` (dstack ROZHODNUTÉ, 2.11.2026: step down within one interval, never announce while
//! a leader is visible). Off by default (`system.fallback.enabled`).
//!
//! - [`config`]: the lenient `system.fallback` block;
//! - [`wire`]: the 32-byte `DSFB` datagram;
//! - [`election`]: the pure role state machine;
//! - [`transport`]: the UDP socket.
//!
//! The controller glue is `crate::controller::fallback`. The library follower
//! (`crates/dsync-ptp`) has none of it on purpose: it serves apps locked to a Dante grandmaster,
//! and a network without Dante PTP plays no Dante audio.

pub mod config;
pub mod election;
pub mod transport;
pub mod wire;

pub use config::FallbackConfig;
pub use election::{Election, Event, Role};
pub use transport::{FallbackTransport, Received, UdpFallbackTransport};
pub use wire::{FallbackSync, NodeId};
