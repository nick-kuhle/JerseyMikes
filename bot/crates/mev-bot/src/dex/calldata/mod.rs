//! Router calldata decoding.
//!
//! New decoders live here so sandwich / arb do not grow a decoder each.
//! The combined entry point is [`crate::strategies::decode_any_router`]: this
//! module must not import `strategies` (that crate-graph edge already points
//! the other way).
//!
//! UniversalRouter is behind `DECODE_UNIVERSAL_ROUTER` at the call site so
//! turning it on is a measured change, not an accidental expansion of the
//! sandwich surface.

pub mod universal_router;

pub use universal_router::{decode as decode_universal_router, UrSwap};
