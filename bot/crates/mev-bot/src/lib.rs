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
//! Nothing in this crate broadcasts a transaction. The only functions that could
//! (`bundle::send_bundle_params`) are behind `Config::live_execution`, which
//! requires two independent environment acknowledgements to enable.

pub mod api;
pub mod bundle;
pub mod config;
pub mod dex;
pub mod engine;
pub mod ingest;
pub mod risk;
pub mod rlp;
pub mod rpc;
pub mod sim;
pub mod signer;
pub mod store;
pub mod strategies;
pub mod types;
