#![deny(warnings)]

//! JerseyMikes MEV bot — simulation-first MEV search on live mainnet data.
//!
//! See `docs/ARCHITECTURE.md` for the wiring diagram. The short version:
//!
//! ```text
//!  mempool ─┐
//!  MEV-Share├─ ingest ─→ strategies ─→ risk ─→ simulate ─→ store ─→ HTTP/SSE ─→ dashboard
//!  heads    │            (sandwich,           (anvil fork
//!  relays  ─┘             jit, arb,            + eth_callBundle)
//!                         liquidation,
//!                         sniper)
//! ```
//!
//! Relay submission is fail-closed behind independent boot arming, broadcast
//! capability, runtime mode, risk, inventory, durable nonce, exact simulation,
//! and strategy-specific qualification gates. All defaults remain shadow-only.

pub mod alerts;
pub mod api;
pub mod attribution;
pub mod bundle;
pub mod competition;
pub mod config;
pub mod dex;
pub mod engine;
pub mod flashblocks;
pub mod ingest;
pub mod inventory;
pub mod latency;
pub mod metrics;
pub mod qualification;
pub mod replay;
pub mod risk;
pub mod rlp;
pub mod rpc;
pub mod signer;
pub mod sim;
/// The directional new-token sniper lane. Deliberately isolated from the
/// atomic profit-or-revert engine above — see `sniper/mod.rs` and
/// `docs/SNIPER.md` for why.
pub mod sniper;
pub mod store;
pub mod strategies;
pub mod submission;
pub mod types;
pub mod valuation;
