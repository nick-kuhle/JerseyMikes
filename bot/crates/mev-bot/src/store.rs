//! SQLite persistence.
//!
//! Writes are small and synchronous (SQLite in WAL mode handles thousands of
//! inserts a second), so the store is a plain mutex-guarded connection rather
//! than an async pool. Every table is append-only, which keeps the dashboard's
//! history queries trivial and makes post-hoc analysis of a run possible.

use std::path::Path;

use alloy_primitives::U256;
use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection};

use crate::types::{now_ms, BundleRecord, Opportunity, SimulationResult, Strategy};

/// One stored anvil-fork simulation, joined to its opportunity, ready for the
/// replay harness to rank against relay bid traces.
#[derive(Clone, Debug)]
pub struct ReplayCandidate {
    pub opportunity_id: String,
    pub strategy: String,
    pub success: bool,
    pub net_wei: i128,
    pub bribe_wei: String,
    pub block_number: u64,
    /// Comma-separated `0x…` hashes; empty means the opportunity had no victim
    /// (arb / liquidation / sniper).
    pub victims: String,
}

#[derive(Clone, Debug)]
pub struct OpportunityVictim {
    pub opportunity_id: String,
    pub strategy: String,
    pub victim_hash: String,
}

#[derive(Clone, Debug)]
pub struct AtomicArbObservation {
    pub opportunity_id: String,
    pub notes: String,
}

#[derive(Clone, Debug)]
pub struct SubmittedBundle {
    pub bundle_id: String,
    pub opportunity_id: String,
    pub target_block: u64,
    pub tx_hashes: Vec<alloy_primitives::B256>,
    pub inclusion_state: String,
    pub observed_block: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct ActualMevMatch {
    pub opportunity_id: String,
    pub block_number: u64,
    pub victim_hash: String,
    pub mev_tx_hashes: Vec<String>,
    pub actor: Option<String>,
    pub gross_weth_wei: U256,
    pub gas_cost_wei: U256,
    pub net_weth_wei: i128,
    pub confidence: String,
    /// Numeric confidence is explicit and machine-comparable; 10_000 is exact.
    pub confidence_score_bps: u64,
    /// Which economic components were observed versus inferred or unavailable.
    pub completeness: serde_json::Value,
    pub evidence: serde_json::Value,
}

#[derive(Clone, Debug, Default)]
pub struct QualificationEvidence {
    pub fork_samples: u64,
    pub relay_errors_bps: Vec<u64>,
    pub actual_errors_bps: Vec<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct ObservationCoverage {
    pub first_seen_ms: Option<u64>,
    pub last_seen_ms: Option<u64>,
    pub maximum_gap_ms: u64,
    pub observations: u64,
}

#[derive(Clone, Debug)]
pub struct ActiveNonceReservation {
    pub bundle_id: String,
    pub start_nonce: u64,
    pub nonce_count: u64,
    pub target_block: u64,
}

/// Durable drawdown-kill-switch snapshot. Singleton row in `risk_state`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PersistedRiskState {
    pub tripped: bool,
    pub tripped_at_ms: Option<u64>,
    pub cumulative_net_wei: i128,
}

pub struct Store {
    /// Visible to the module so [`AsyncStore`]'s writer task can hold the
    /// connection across a whole batch transaction.
    pub(crate) conn: Mutex<Connection>,
}

/// Aggregated numbers the dashboard shows.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PnlSummary {
    pub strategy: String,
    pub simulations: i64,
    pub wins: i64,
    pub losses: i64,
    pub gross_profit_wei: String,
    pub gas_spent_wei: String,
    pub net_profit_wei: String,
    pub best_net_wei: String,
    pub worst_net_wei: String,
    pub avg_latency_ms: f64,
}

impl Store {
    pub fn open(path: &str) -> Result<Self> {
        if let Some(parent) = Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        // WAL lets readers (the dashboard) run against a snapshot while the
        // writer task commits, instead of blocking each other.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // NORMAL: the writer does not wait for an fsync on every commit. On a
        // crash the last few telemetry rows can be lost, which is acceptable
        // for observability data and is the difference between a commit
        // costing microseconds and costing a disk revolution.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // Keep the WAL from growing without bound between checkpoints while
        // still letting a batch commit without an immediate checkpoint.
        conn.pragma_update(None, "wal_autocheckpoint", 1_000)?;
        // A batch commit briefly contends with dashboard reads; wait rather
        // than fail with SQLITE_BUSY.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // Larger page cache (~8 MB) and in-memory temp tables: the dashboard's
        // aggregate queries are the main readers and they scan.
        conn.pragma_update(None, "cache_size", -8_000)?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.lock().execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS opportunities (
                id            TEXT PRIMARY KEY,
                strategy      TEXT NOT NULL,
                target_block  INTEGER NOT NULL,
                profit_token  TEXT NOT NULL,
                expected_wei  TEXT NOT NULL,
                notional_wei  TEXT NOT NULL,
                victims       TEXT NOT NULL,
                notes         TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS simulations (
                id             INTEGER PRIMARY KEY AUTOINCREMENT,
                opportunity_id TEXT NOT NULL,
                strategy       TEXT NOT NULL,
                backend        TEXT NOT NULL,
                success        INTEGER NOT NULL,
                gross_wei      TEXT NOT NULL,
                gas_used       INTEGER NOT NULL,
                gas_cost_wei   TEXT NOT NULL,
                bribe_wei      TEXT NOT NULL,
                net_wei        TEXT NOT NULL,
                revert_reason  TEXT,
                target_block   INTEGER NOT NULL,
                latency_ms     INTEGER NOT NULL,
                created_at_ms  INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS bundles (
                id             TEXT PRIMARY KEY,
                opportunity_id TEXT NOT NULL,
                strategy       TEXT NOT NULL,
                target_block   INTEGER NOT NULL,
                tx_count       INTEGER NOT NULL,
                submitted      INTEGER NOT NULL,
                payload        TEXT NOT NULL,
                created_at_ms  INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS blocks (
                number        INTEGER PRIMARY KEY,
                hash          TEXT NOT NULL,
                parent_hash   TEXT NOT NULL DEFAULT '',
                canonical     INTEGER NOT NULL DEFAULT 1,
                base_fee_wei  TEXT NOT NULL,
                gas_used      INTEGER NOT NULL,
                timestamp     INTEGER NOT NULL,
                seen_at_ms    INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS reorgs (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                from_block    INTEGER NOT NULL,
                to_block      INTEGER NOT NULL,
                depth         INTEGER NOT NULL,
                old_hash      TEXT NOT NULL,
                new_hash      TEXT NOT NULL,
                seen_at_ms    INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS reconciliations (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                block_number    INTEGER NOT NULL,
                opportunity_id  TEXT NOT NULL,
                strategy        TEXT NOT NULL,
                sim_net_wei     TEXT NOT NULL,
                our_bribe_wei   TEXT NOT NULL,
                winning_bid_wei TEXT NOT NULL,
                victim_landed   INTEGER NOT NULL,
                would_outbid    INTEGER NOT NULL,
                inclusion_p     REAL NOT NULL,
                true_positive   INTEGER NOT NULL,
                false_positive  INTEGER NOT NULL,
                reorged         INTEGER NOT NULL DEFAULT 0,
                created_at_ms   INTEGER NOT NULL,
                UNIQUE(opportunity_id, block_number)
            );

            CREATE TABLE IF NOT EXISTS execution_outcomes (
                bundle_id TEXT PRIMARY KEY,
                opportunity_id TEXT NOT NULL,
                block_number INTEGER NOT NULL,
                tx_hashes TEXT NOT NULL,
                gross_profit_wei TEXT NOT NULL,
                bribe_wei TEXT NOT NULL,
                retained_profit_wei TEXT NOT NULL,
                gas_cost_wei TEXT NOT NULL,
                net_profit_wei TEXT NOT NULL,
                canonical INTEGER NOT NULL DEFAULT 1,
                created_at_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS relay_submissions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                bundle_id TEXT NOT NULL,
                opportunity_id TEXT NOT NULL,
                relay TEXT NOT NULL,
                accepted INTEGER NOT NULL,
                response TEXT NOT NULL,
                submitted_at_ms INTEGER NOT NULL,
                UNIQUE(bundle_id, relay)
            );

            CREATE TABLE IF NOT EXISTS qualification_incidents (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                kind TEXT NOT NULL,
                detail TEXT NOT NULL,
                occurred_at_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS nonce_reservations (
                bundle_id TEXT PRIMARY KEY,
                opportunity_id TEXT NOT NULL,
                start_nonce INTEGER NOT NULL,
                nonce_count INTEGER NOT NULL,
                target_block INTEGER NOT NULL,
                status TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS actual_mev_matches (
                opportunity_id TEXT PRIMARY KEY,
                block_number INTEGER NOT NULL,
                victim_hash TEXT NOT NULL,
                mev_tx_hashes TEXT NOT NULL,
                actor TEXT,
                gross_weth_wei TEXT NOT NULL,
                gas_cost_wei TEXT NOT NULL,
                net_weth_wei TEXT NOT NULL,
                confidence TEXT NOT NULL,
                evidence TEXT NOT NULL,
                canonical INTEGER NOT NULL DEFAULT 1,
                created_at_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS relay_bids (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                relay         TEXT NOT NULL,
                slot          INTEGER NOT NULL,
                builder       TEXT NOT NULL,
                value_wei     TEXT NOT NULL,
                seen_at_ms    INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS relay_blocks (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                relay         TEXT NOT NULL,
                slot          INTEGER NOT NULL,
                block_number  INTEGER NOT NULL,
                block_hash    TEXT NOT NULL,
                builder       TEXT NOT NULL,
                value_wei     TEXT NOT NULL,
                gas_used      INTEGER NOT NULL,
                num_tx        INTEGER NOT NULL,
                seen_at_ms    INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS relay_block_txs (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                block_number  INTEGER NOT NULL,
                tx_index      INTEGER NOT NULL,
                hash          TEXT NOT NULL,
                from_addr     TEXT,
                to_addr       TEXT,
                value_wei     TEXT NOT NULL,
                nonce         INTEGER NOT NULL,
                gas           INTEGER NOT NULL,
                selector      TEXT,
                input         TEXT NOT NULL,
                UNIQUE(block_number, hash)
            );

            CREATE INDEX IF NOT EXISTS idx_sim_strategy ON simulations(strategy);
            CREATE INDEX IF NOT EXISTS idx_sim_created ON simulations(created_at_ms);
            CREATE INDEX IF NOT EXISTS idx_sim_qualification
                ON simulations(strategy, backend, created_at_ms, success, opportunity_id, id);
            CREATE INDEX IF NOT EXISTS idx_sim_opportunity_backend
                ON simulations(opportunity_id, backend, id);
            CREATE INDEX IF NOT EXISTS idx_blocks_coverage ON blocks(canonical, seen_at_ms);
            CREATE INDEX IF NOT EXISTS idx_qualification_incident_time
                ON qualification_incidents(occurred_at_ms);
            CREATE INDEX IF NOT EXISTS idx_opp_created ON opportunities(created_at_ms);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_relay_slot ON relay_bids(relay, slot);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_relay_block_slot ON relay_blocks(relay, slot);
            CREATE INDEX IF NOT EXISTS idx_relay_block_txs_block ON relay_block_txs(block_number);
            CREATE INDEX IF NOT EXISTS idx_recon_block ON reconciliations(block_number);
            CREATE INDEX IF NOT EXISTS idx_actual_mev_block ON actual_mev_matches(block_number);
            CREATE INDEX IF NOT EXISTS idx_submission_bundle ON relay_submissions(bundle_id);

            -- Singleton safety state. The drawdown kill switch used to live
            -- only in process memory; a restart silently re-armed a tripped
            -- bot. Row id is constrained to 1 so this cannot become a log.
            CREATE TABLE IF NOT EXISTS risk_state (
                id                    INTEGER PRIMARY KEY CHECK (id = 1),
                kill_switch_tripped   INTEGER NOT NULL DEFAULT 0,
                tripped_at_ms         INTEGER,
                cumulative_net_wei       TEXT NOT NULL DEFAULT '0',
                live_smoke_used          INTEGER NOT NULL DEFAULT 0,
                live_smoke_gas_risk_wei  TEXT NOT NULL DEFAULT '0'
            );
            INSERT OR IGNORE INTO risk_state (id, kill_switch_tripped, cumulative_net_wei)
                VALUES (1, 0, '0');

            -- Sequencer-backend qualification evidence: for a victim-pinned
            -- opportunity, the fork's predicted victim-leg delta vs the
            -- victim's realised delta in the canonical (included) block.
            -- This is the "included block" second opinion that replaces the
            -- relay eth_callBundle comparison on chains without relays.
            CREATE TABLE IF NOT EXISTS block_comparisons (
                id             INTEGER PRIMARY KEY AUTOINCREMENT,
                opportunity_id TEXT NOT NULL,
                strategy       TEXT NOT NULL,
                victim_hash    TEXT NOT NULL,
                block_number   INTEGER NOT NULL,
                predicted_wei  TEXT NOT NULL,
                realized_wei   TEXT NOT NULL,
                error_bps      INTEGER NOT NULL,
                created_at_ms  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_block_comparisons_strategy
                ON block_comparisons (strategy, created_at_ms);

            -- Victimless independent second opinion (WS-R). One row is one
            -- (opportunity, state, route, amount, direction) sample and cannot
            -- be reused as a corresponding-outcome match.
            CREATE TABLE IF NOT EXISTS state_comparisons (
                id               TEXT PRIMARY KEY,
                opportunity_id   TEXT NOT NULL,
                strategy         TEXT NOT NULL,
                source_state_id  TEXT NOT NULL,
                canonical_block  INTEGER NOT NULL,
                canonical_hash   TEXT NOT NULL DEFAULT '',
                route            TEXT NOT NULL,
                amount_in        TEXT NOT NULL,
                direction        TEXT NOT NULL,
                predicted_wei    TEXT NOT NULL,
                realized_wei     TEXT NOT NULL,
                error_bps        INTEGER NOT NULL,
                canonical        INTEGER NOT NULL DEFAULT 1,
                created_at_ms    INTEGER NOT NULL,
                UNIQUE(opportunity_id, source_state_id, route, amount_in, direction)
            );
            CREATE INDEX IF NOT EXISTS idx_state_comparisons_strategy
                ON state_comparisons (strategy, created_at_ms, canonical);

            -- ---------------------------------------------------------------
            -- Directional sniper lane (isolated; see docs/SNIPER.md).
            --
            -- These two tables are the ONLY durable state the sniper owns, and
            -- nothing else in the schema references them. An operator who
            -- wants the lane gone can drop both without affecting a single
            -- atomic-path query.
            --
            -- Open exposure MUST survive a restart: an unmanaged open position
            -- is the worst failure mode this lane has, so entries are written
            -- before the buy is submitted, not after it confirms.
            CREATE TABLE IF NOT EXISTS sniper_positions (
                id                TEXT PRIMARY KEY,
                chain_id          INTEGER NOT NULL,
                token             TEXT NOT NULL,
                pair              TEXT NOT NULL,
                venue             TEXT NOT NULL,
                state             TEXT NOT NULL,
                trigger_tx        TEXT,
                entry_tx          TEXT,
                entry_cost_wei    TEXT NOT NULL,
                entry_qty         TEXT NOT NULL,
                remaining_qty     TEXT NOT NULL,
                realized_wei      TEXT NOT NULL DEFAULT '0',
                gas_spent_wei     TEXT NOT NULL DEFAULT '0',
                peak_value_wei    TEXT NOT NULL DEFAULT '0',
                opened_block      INTEGER NOT NULL,
                opened_at_ms      INTEGER NOT NULL,
                closed_at_ms      INTEGER,
                exit_reason       TEXT,
                entry_verdict     TEXT NOT NULL DEFAULT 'unknown',
                notes             TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS idx_sniper_positions_state
                ON sniper_positions (state, opened_at_ms);
            CREATE INDEX IF NOT EXISTS idx_sniper_positions_token
                ON sniper_positions (token);

            -- Every entry and exit fill, append-only. The position row is a
            -- projection of these; keeping the fills lets an operator audit
            -- how a position actually got to its current shape.
            CREATE TABLE IF NOT EXISTS sniper_fills (
                id             TEXT PRIMARY KEY,
                position_id    TEXT NOT NULL,
                side           TEXT NOT NULL,          -- 'buy' | 'sell'
                reason         TEXT NOT NULL DEFAULT '',
                qty            TEXT NOT NULL,
                weth_wei       TEXT NOT NULL,
                gas_wei        TEXT NOT NULL DEFAULT '0',
                tx_hash        TEXT,
                block_number   INTEGER,
                created_at_ms  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_sniper_fills_position
                ON sniper_fills (position_id, created_at_ms);

            -- Honeypot verdicts, so a token is probed once and remembered.
            -- Doubles as the evidence population for the sniper's own
            -- (non-blocking) qualification track.
            CREATE TABLE IF NOT EXISTS sniper_token_verdicts (
                token           TEXT PRIMARY KEY,
                chain_id        INTEGER NOT NULL,
                verdict         TEXT NOT NULL,
                round_trip_bps  INTEGER,
                probed_at_ms    INTEGER NOT NULL,
                detail          TEXT NOT NULL DEFAULT ''
            );
            "#,
        )?;
        // Additive columns for databases created before Phase 1. SQLite has no
        // IF NOT EXISTS for columns; a duplicate-column error is the success
        // case on a second boot.
        self.add_column("blocks", "parent_hash", "TEXT NOT NULL DEFAULT ''");
        self.add_column("blocks", "canonical", "INTEGER NOT NULL DEFAULT 1");
        self.add_column("simulations", "reorged", "INTEGER NOT NULL DEFAULT 0");
        self.add_column("simulations", "victim_predicted_out_wei", "TEXT");
        self.add_column("bundles", "included", "INTEGER");
        self.add_column("bundles", "included_block", "INTEGER");
        self.add_column("bundles", "inclusion_checked_ms", "INTEGER");
        self.add_column(
            "bundles",
            "inclusion_state",
            "TEXT NOT NULL DEFAULT 'pending'",
        );
        self.add_column("bundles", "observed_tx_hashes", "TEXT");
        self.add_column("execution_outcomes", "finalized_block", "INTEGER");
        self.add_column(
            "actual_mev_matches",
            "confidence_score_bps",
            "INTEGER NOT NULL DEFAULT 0",
        );
        self.add_column(
            "actual_mev_matches",
            "completeness",
            "TEXT NOT NULL DEFAULT '{}'",
        );
        self.add_column(
            "risk_state",
            "live_smoke_used",
            "INTEGER NOT NULL DEFAULT 0",
        );
        self.add_column(
            "risk_state",
            "live_smoke_gas_risk_wei",
            "TEXT NOT NULL DEFAULT '0'",
        );

        // --- two-ledger provenance (additive; work order C.1) -------------
        // Every position/fill carries its execution domain so the simulation
        // and live portfolios can never be merged without a label. Defaults
        // match pre-provenance rows (live-shaped); rows whose notes prove a
        // paper origin are backfilled below.
        self.add_column(
            "sniper_positions",
            "execution_mode",
            "TEXT NOT NULL DEFAULT 'live'",
        );
        self.add_column(
            "sniper_positions",
            "settlement",
            "TEXT NOT NULL DEFAULT 'on_chain'",
        );
        self.add_column(
            "sniper_positions",
            "tx_status",
            "TEXT NOT NULL DEFAULT 'mined'",
        );
        self.add_column(
            "sniper_fills",
            "execution_mode",
            "TEXT NOT NULL DEFAULT 'live'",
        );
        self.add_column("sniper_positions", "exit_tx", "TEXT");
        self.conn.lock().execute_batch(
            "UPDATE sniper_positions
                SET execution_mode = 'simulation', settlement = 'paper'
              WHERE execution_mode = 'live' AND notes LIKE 'SIMULATION%';
             UPDATE sniper_fills
                SET execution_mode = 'simulation'
              WHERE execution_mode = 'live' AND reason = 'simulation';",
        )?;

        // First-class simulation ledger: the paper bankroll survives restarts
        // and an explicit reset rewrites it rather than silently mutating an
        // in-memory number.
        self.conn.lock().execute_batch(
            "CREATE TABLE IF NOT EXISTS sniper_simulation_state (
                id            INTEGER PRIMARY KEY CHECK (id = 1),
                balance_wei   TEXT NOT NULL,
                reset_at_ms   INTEGER NOT NULL DEFAULT 0,
                updated_at_ms INTEGER NOT NULL DEFAULT 0
            );",
        )?;
        Ok(())
    }

    fn add_column(&self, table: &str, column: &str, decl: &str) {
        let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {decl}");
        if let Err(e) = self.conn.lock().execute(&sql, []) {
            let msg = e.to_string().to_ascii_lowercase();
            if !msg.contains("duplicate column") {
                tracing::debug!(table, column, error = %e, "alter column skipped");
            }
        }
    }

    pub fn record_opportunity(&self, o: &Opportunity) -> Result<()> {
        write_opportunity(&self.conn.lock(), o)
    }

    pub fn record_simulation(&self, s: &SimulationResult) -> Result<()> {
        write_simulation(&self.conn.lock(), s)
    }

    pub fn record_bundle(&self, b: &BundleRecord) -> Result<()> {
        write_bundle(&self.conn.lock(), b)
    }

    pub fn record_block(&self, head: &crate::types::BlockHead) -> Result<()> {
        write_block(&self.conn.lock(), head)
    }
}

/// One deferred append-only write.
///
/// The hot path produces these and drops them on a channel; the writer task
/// drains them and commits a whole batch inside a single transaction.
#[derive(Debug)]
pub enum WriteOp {
    Opportunity(Box<Opportunity>),
    Simulation(Box<SimulationResult>),
    Bundle(Box<BundleRecord>),
    Block(Box<crate::types::BlockHead>),
}

impl WriteOp {
    fn apply(&self, conn: &Connection) -> Result<()> {
        match self {
            WriteOp::Opportunity(o) => write_opportunity(conn, o),
            WriteOp::Simulation(s) => write_simulation(conn, s),
            WriteOp::Bundle(b) => write_bundle(conn, b),
            WriteOp::Block(h) => write_block(conn, h),
        }
    }
}

/// Asynchronous, batching front end to [`Store`].
///
/// Every opportunity, simulation and bundle used to be written with a blocking
/// `INSERT` from whatever task produced it — inside the latency-critical
/// evaluation path, and with every one of those tasks contending on the single
/// connection mutex. Each insert is its own implicit transaction, so a busy
/// block turned into thousands of separate commits, each one taking the mutex
/// and touching the WAL.
///
/// Here the producer does a non-blocking `send` onto a bounded channel and
/// moves on. One background task drains up to [`BATCH_MAX`] operations and
/// commits them inside a **single** transaction, which is where nearly all of
/// the saving is: one fsync-class commit instead of N.
///
/// Back-pressure is deliberate: the channel is bounded, and when it is full
/// the write is dropped and counted rather than blocking the hot path.
/// Persistence here is observability, not settlement — the bot's decisions do
/// not depend on these rows, so trading a dropped telemetry row for keeping
/// the searcher on-time is the right trade. `dropped()` makes it visible.
pub struct AsyncStore {
    tx: tokio::sync::mpsc::Sender<WriteOp>,
    /// First unpersisted drop timestamp. The existing writer records it after
    /// draining a batch, avoiding a second thread solely for rare incidents.
    incident_at_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    dropped: std::sync::atomic::AtomicU64,
    queued: std::sync::atomic::AtomicU64,
}

/// Largest number of operations committed in one transaction.
const BATCH_MAX: usize = 256;

impl AsyncStore {
    /// Spawn the writer task against an existing store.
    ///
    /// `capacity` bounds the queue; the store handle stays usable for reads
    /// and for the synchronous writes that are not on the hot path.
    pub fn spawn(store: std::sync::Arc<Store>, capacity: usize) -> std::sync::Arc<Self> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<WriteOp>(capacity.max(64));
        let incident_at_ms = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let this = std::sync::Arc::new(Self {
            tx,
            incident_at_ms: incident_at_ms.clone(),
            dropped: std::sync::atomic::AtomicU64::new(0),
            queued: std::sync::atomic::AtomicU64::new(0),
        });
        let writer_incident_at_ms = incident_at_ms;
        tokio::task::spawn_blocking(move || {
            let mut batch: Vec<WriteOp> = Vec::with_capacity(BATCH_MAX);
            // `blocking_recv` parks this dedicated thread, never a runtime
            // worker, so a slow disk cannot stall async tasks.
            while let Some(first) = rx.blocking_recv() {
                batch.push(first);
                // Opportunistically drain whatever else is already queued:
                // under load this is what turns N commits into one.
                while batch.len() < BATCH_MAX {
                    match rx.try_recv() {
                        Ok(op) => batch.push(op),
                        Err(_) => break,
                    }
                }
                let mut conn = store.conn.lock();
                match conn.transaction() {
                    Ok(txn) => {
                        for op in &batch {
                            if let Err(e) = op.apply(&txn) {
                                tracing::debug!(target: "store", error = %e, "deferred write failed");
                            }
                        }
                        if let Err(e) = txn.commit() {
                            tracing::warn!(target: "store", error = %e, "batch commit failed");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(target: "store", error = %e, "could not open write transaction");
                    }
                }
                let incident_at =
                    writer_incident_at_ms.swap(0, std::sync::atomic::Ordering::AcqRel);
                if incident_at != 0 {
                    if let Err(error) = conn.execute(
                        "INSERT INTO qualification_incidents (kind, detail, occurred_at_ms) VALUES (?1,?2,?3)",
                        params![
                            "persistence_drop",
                            "one or more bounded telemetry writes were dropped",
                            incident_at as i64
                        ],
                    ) {
                        writer_incident_at_ms.store(
                            incident_at,
                            std::sync::atomic::Ordering::Release,
                        );
                        tracing::error!(target: "store", %error, "could not persist qualification incident");
                    }
                }
                drop(conn);
                batch.clear();
            }
            tracing::info!(target: "store", "writer task stopped");
        });
        this
    }

    fn mark_drop_incident(&self) {
        let _ = self.incident_at_ms.compare_exchange(
            0,
            now_ms(),
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        );
    }

    /// Queue a write. Never blocks and never fails the caller: a full queue
    /// increments `dropped()` instead of stalling the searcher.
    pub fn send(&self, op: WriteOp) {
        use std::sync::atomic::Ordering::Relaxed;
        match self.tx.try_send(op) {
            Ok(()) => {
                self.queued.fetch_add(1, Relaxed);
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                let n = self.dropped.fetch_add(1, Relaxed) + 1;
                // Rate-limited: one line per 1000 drops, so a sustained
                // overload cannot itself become the bottleneck.
                self.mark_drop_incident();
                if n % 1000 == 1 {
                    tracing::warn!(
                        target: "store",
                        dropped = n,
                        "persistence queue full — dropping telemetry writes to protect the hot path"
                    );
                }
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                self.dropped.fetch_add(1, Relaxed);
                self.mark_drop_incident();
            }
        }
    }

    pub fn record_opportunity(&self, o: &Opportunity) {
        self.send(WriteOp::Opportunity(Box::new(o.clone())));
    }

    pub fn record_simulation(&self, s: &SimulationResult) {
        self.send(WriteOp::Simulation(Box::new(s.clone())));
    }

    pub fn record_bundle(&self, b: &BundleRecord) {
        self.send(WriteOp::Bundle(Box::new(b.clone())));
    }

    pub fn record_block(&self, head: &crate::types::BlockHead) {
        self.send(WriteOp::Block(Box::new(head.clone())));
    }

    /// Writes discarded because the queue was full.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Writes accepted onto the queue.
    pub fn queued(&self) -> u64 {
        self.queued.load(std::sync::atomic::Ordering::Relaxed)
    }
}

fn write_opportunity(conn: &Connection, o: &Opportunity) -> Result<()> {
    conn.execute(
            "INSERT OR REPLACE INTO opportunities
             (id, strategy, target_block, profit_token, expected_wei, notional_wei, victims, notes, created_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                o.id,
                o.strategy.as_str(),
                o.target_block as i64,
                format!("{:?}", o.profit_token),
                o.expected_profit_wei.to_string(),
                o.notional_wei.to_string(),
                o.victim_hashes
                    .iter()
                    .map(|h| format!("{h:?}"))
                    .collect::<Vec<_>>()
                    .join(","),
                o.notes,
                o.created_at_ms as i64,
            ],
        )?;
    Ok(())
}

fn write_simulation(conn: &Connection, s: &SimulationResult) -> Result<()> {
    conn.execute(
            "INSERT INTO simulations
             (opportunity_id, strategy, backend, success, gross_wei, gas_used, gas_cost_wei, bribe_wei,
              net_wei, victim_predicted_out_wei, revert_reason, target_block, latency_ms, created_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                s.opportunity_id,
                s.strategy.as_str(),
                s.backend.as_str(),
                s.success as i32,
                s.gross_profit_wei.to_string(),
                s.gas_used as i64,
                s.gas_cost_wei.to_string(),
                s.bribe_wei.to_string(),
                s.net_profit_wei.to_string(),
                s.victim_predicted_out_wei,
                s.revert_reason,
                s.target_block as i64,
                s.sim_latency_ms as i64,
                s.created_at_ms as i64,
            ],
        )?;
    Ok(())
}

fn write_bundle(conn: &Connection, b: &BundleRecord) -> Result<()> {
    conn.execute(
            "INSERT OR REPLACE INTO bundles
             (id, opportunity_id, strategy, target_block, tx_count, submitted, payload, created_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                b.id,
                b.opportunity_id,
                b.strategy.as_str(),
                b.target_block as i64,
                b.txs.len() as i64,
                b.submitted as i32,
                serde_json::to_string(&crate::bundle::send_bundle_params(b)).unwrap_or_default(),
                b.created_at_ms as i64,
            ],
        )?;
    Ok(())
}

fn write_block(conn: &Connection, head: &crate::types::BlockHead) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO blocks
             (number, hash, parent_hash, canonical, base_fee_wei, gas_used, timestamp, seen_at_ms)
             VALUES (?1,?2,?3,1,?4,?5,?6,?7)",
        params![
            head.number as i64,
            format!("{:?}", head.hash),
            format!("{:?}", head.parent_hash),
            head.base_fee_per_gas.to_string(),
            head.gas_used as i64,
            head.timestamp as i64,
            now_ms() as i64,
        ],
    )?;
    Ok(())
}

impl Store {
    /// Mark simulations (and reconciliations) in `[from_block, to_block]` as
    /// belonging to a discarded fork, and log the re-org.
    pub fn record_reorg(
        &self,
        from_block: u64,
        to_block: u64,
        old_hash: &str,
        new_hash: &str,
    ) -> Result<()> {
        let depth = to_block.saturating_sub(from_block).saturating_add(1);
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE simulations SET reorged = 1
             WHERE target_block >= ?1 AND target_block <= ?2 AND COALESCE(reorged, 0) = 0",
            params![from_block as i64, to_block as i64],
        )?;
        conn.execute(
            "UPDATE reconciliations SET reorged = 1
             WHERE block_number >= ?1 AND block_number <= ?2 AND COALESCE(reorged, 0) = 0",
            params![from_block as i64, to_block as i64],
        )?;
        conn.execute(
            "UPDATE execution_outcomes SET canonical = 0
             WHERE block_number >= ?1 AND block_number <= ?2 AND canonical = 1",
            params![from_block as i64, to_block as i64],
        )?;
        conn.execute(
            "UPDATE actual_mev_matches SET canonical = 0
             WHERE block_number >= ?1 AND block_number <= ?2 AND canonical = 1",
            params![from_block as i64, to_block as i64],
        )?;
        conn.execute(
            "UPDATE state_comparisons SET canonical = 0
             WHERE canonical_block >= ?1 AND canonical_block <= ?2 AND canonical = 1",
            params![from_block as i64, to_block as i64],
        )?;
        conn.execute(
            "UPDATE bundles SET included = NULL, inclusion_state = 'reorged_pending'
             WHERE included_block >= ?1 AND included_block <= ?2",
            params![from_block as i64, to_block as i64],
        )?;
        conn.execute(
            "UPDATE nonce_reservations SET status = 'reorged', updated_at_ms = ?3
             WHERE bundle_id IN (
               SELECT id FROM bundles WHERE included_block >= ?1 AND included_block <= ?2
             )",
            params![from_block as i64, to_block as i64, now_ms() as i64],
        )?;
        conn.execute(
            "UPDATE blocks SET canonical = 0 WHERE number >= ?1 AND number <= ?2",
            params![from_block as i64, to_block as i64],
        )?;
        conn.execute(
            "INSERT INTO reorgs (from_block, to_block, depth, old_hash, new_hash, seen_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                from_block as i64,
                to_block as i64,
                depth as i64,
                old_hash,
                new_hash,
                now_ms() as i64,
            ],
        )?;
        Ok(())
    }

    pub fn recent_reorgs(&self, limit: i64) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT from_block, to_block, depth, old_hash, new_hash, seen_at_ms
             FROM reorgs ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok(serde_json::json!({
                "fromBlock": row.get::<_, i64>(0)?,
                "toBlock": row.get::<_, i64>(1)?,
                "depth": row.get::<_, i64>(2)?,
                "oldHash": row.get::<_, String>(3)?,
                "newHash": row.get::<_, String>(4)?,
                "seenAtMs": row.get::<_, i64>(5)?,
            }))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Realised builder payment for a block, if any relay has reported one.
    /// When several relays delivered the same number we take the highest bid:
    /// that is the market-clearing price the competition model ranks against.
    pub fn winning_bid_for_block(&self, block_number: u64) -> Result<Option<U256>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT value_wei FROM relay_blocks
             WHERE block_number = ?1
             ORDER BY length(value_wei) DESC, value_wei DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![block_number as i64])?;
        if let Some(row) = rows.next()? {
            let s: String = row.get(0)?;
            Ok(s.parse::<U256>().ok())
        } else {
            Ok(None)
        }
    }

    /// True if any of the comma-separated victim hashes landed in `block_number`.
    pub fn any_victim_landed(&self, block_number: u64, victims: &str) -> Result<bool> {
        let hashes: Vec<&str> = victims
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if hashes.is_empty() {
            return Ok(false);
        }
        let conn = self.conn.lock();
        for h in hashes {
            let found: i64 = conn.query_row(
                "SELECT COUNT(*) FROM relay_block_txs WHERE block_number = ?1 AND hash = ?2",
                params![block_number as i64, h],
                |r| r.get(0),
            )?;
            if found > 0 {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Anvil-fork simulations in a block window, newest first. Re-orged rows
    /// are excluded so a discarded fork cannot inflate the true-positive rate.
    pub fn replay_candidates(
        &self,
        from_block: Option<u64>,
        to_block: Option<u64>,
        limit: i64,
    ) -> Result<Vec<ReplayCandidate>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT s.opportunity_id, s.strategy, s.success, CAST(s.net_wei AS TEXT), s.bribe_wei,
                    s.target_block, COALESCE(o.victims, '')
             FROM simulations s
             LEFT JOIN opportunities o ON o.id = s.opportunity_id
             WHERE s.backend = 'anvil_fork'
               AND COALESCE(s.reorged, 0) = 0
               AND (?1 IS NULL OR s.target_block >= ?1)
               AND (?2 IS NULL OR s.target_block <= ?2)
             ORDER BY s.target_block DESC, s.id DESC
             LIMIT ?3",
        )?;
        let from = from_block.map(|n| n as i64);
        let to = to_block.map(|n| n as i64);
        let rows = stmt.query_map(params![from, to, limit], |row| {
            Ok(ReplayCandidate {
                opportunity_id: row.get(0)?,
                strategy: row.get(1)?,
                success: row.get::<_, i64>(2)? == 1,
                net_wei: parse_i128_decimal(&row.get::<_, String>(3)?),
                bribe_wei: row.get(4)?,
                block_number: row.get::<_, i64>(5)? as u64,
                victims: row.get(6)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_reconciliation(
        &self,
        block_number: u64,
        opportunity_id: &str,
        strategy: &str,
        sim_net_wei: i128,
        our_bribe_wei: &str,
        winning_bid_wei: &str,
        victim_landed: bool,
        would_outbid: bool,
        inclusion_p: f64,
        true_positive: bool,
        false_positive: bool,
    ) -> Result<()> {
        self.conn.lock().execute(
            "INSERT OR REPLACE INTO reconciliations
             (block_number, opportunity_id, strategy, sim_net_wei, our_bribe_wei, winning_bid_wei,
              victim_landed, would_outbid, inclusion_p, true_positive, false_positive, reorged, created_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,0,?12)",
            params![
                block_number as i64,
                opportunity_id,
                strategy,
                sim_net_wei.to_string(),
                our_bribe_wei,
                winning_bid_wei,
                victim_landed as i32,
                would_outbid as i32,
                inclusion_p,
                true_positive as i32,
                false_positive as i32,
                now_ms() as i64,
            ],
        )?;
        Ok(())
    }

    pub fn recent_reconciliations(&self, limit: i64) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT block_number, opportunity_id, strategy, CAST(sim_net_wei AS TEXT), our_bribe_wei, winning_bid_wei,
                    victim_landed, would_outbid, inclusion_p, true_positive, false_positive, reorged, created_at_ms
             FROM reconciliations
             WHERE COALESCE(reorged, 0) = 0
             ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok(serde_json::json!({
                "blockNumber": row.get::<_, i64>(0)?,
                "opportunityId": row.get::<_, String>(1)?,
                "strategy": row.get::<_, String>(2)?,
                "simNetWei": row.get::<_, String>(3)?,
                "ourBribeWei": row.get::<_, String>(4)?,
                "winningBidWei": row.get::<_, String>(5)?,
                "victimLanded": row.get::<_, i64>(6)? == 1,
                "wouldOutbid": row.get::<_, i64>(7)? == 1,
                "inclusionP": row.get::<_, f64>(8)?,
                "truePositive": row.get::<_, i64>(9)? == 1,
                "falsePositive": row.get::<_, i64>(10)? == 1,
                "reorged": row.get::<_, i64>(11)? == 1,
                "createdAtMs": row.get::<_, i64>(12)?,
            }))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Aggregate inclusion stats across canonical reconciliations.
    pub fn competition_summary(&self) -> Result<serde_json::Value> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(true_positive), 0),
                    COALESCE(SUM(false_positive), 0),
                    COALESCE(SUM(would_outbid), 0),
                    COALESCE(SUM(victim_landed), 0),
                    COALESCE(AVG(inclusion_p), 0)
             FROM reconciliations WHERE COALESCE(reorged, 0) = 0",
            [],
            |r| {
                Ok(serde_json::json!({
                    "rows": r.get::<_, i64>(0)?,
                    "truePositives": r.get::<_, i64>(1)?,
                    "falsePositives": r.get::<_, i64>(2)?,
                    "wouldOutbid": r.get::<_, i64>(3)?,
                    "victimsLanded": r.get::<_, i64>(4)?,
                    "meanInclusionP": r.get::<_, f64>(5)?,
                }))
            },
        )
        .map_err(Into::into)
    }

    pub fn record_relay_bid(
        &self,
        relay: &str,
        slot: u64,
        builder: &str,
        value: U256,
    ) -> Result<()> {
        self.conn.lock().execute(
            "INSERT OR IGNORE INTO relay_bids (relay, slot, builder, value_wei, seen_at_ms)
             VALUES (?1,?2,?3,?4,?5)",
            params![
                relay,
                slot as i64,
                builder,
                value.to_string(),
                crate::types::now_ms() as i64
            ],
        )?;
        Ok(())
    }

    pub fn submitted_bundles_through(&self, block_number: u64) -> Result<Vec<SubmittedBundle>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, opportunity_id, target_block, payload,
                    inclusion_state, included_block FROM bundles
             WHERE submitted = 1 AND included IS NULL AND target_block <= ?1
             ORDER BY target_block ASC LIMIT 100",
        )?;
        let rows = stmt.query_map(params![block_number as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u64,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<i64>>(5)?
                    .map(|value| value.max(0) as u64),
            ))
        })?;
        let mut out = Vec::new();
        for (bundle_id, opportunity_id, target_block, payload, inclusion_state, observed_block) in
            rows.flatten()
        {
            let value: serde_json::Value = serde_json::from_str(&payload).unwrap_or_default();
            let tx_hashes = value
                .get(0)
                .and_then(|entry| entry.get("txs"))
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .filter_map(|raw| hex::decode(raw.trim_start_matches("0x")).ok())
                .map(alloy_primitives::keccak256)
                .collect::<Vec<_>>();
            if !tx_hashes.is_empty() {
                out.push(SubmittedBundle {
                    bundle_id,
                    opportunity_id,
                    target_block,
                    tx_hashes,
                    inclusion_state,
                    observed_block,
                });
            }
        }
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_execution_outcome(
        &self,
        bundle: &SubmittedBundle,
        block_number: u64,
        finalized_block: u64,
        gross_profit: U256,
        bribe: U256,
        retained_profit: U256,
        gas_cost: U256,
        net_profit: i128,
    ) -> Result<()> {
        let mut conn = self.conn.lock();
        let txn = conn.transaction()?;
        txn.execute(
            "INSERT OR REPLACE INTO execution_outcomes
             (bundle_id, opportunity_id, block_number, tx_hashes, gross_profit_wei,
              bribe_wei, retained_profit_wei, gas_cost_wei, net_profit_wei,
              canonical, created_at_ms, finalized_block)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,1,?10,?11)",
            params![
                bundle.bundle_id,
                bundle.opportunity_id,
                block_number as i64,
                serde_json::to_string(
                    &bundle
                        .tx_hashes
                        .iter()
                        .map(|hash| format!("{hash:?}"))
                        .collect::<Vec<_>>()
                )?,
                gross_profit.to_string(),
                bribe.to_string(),
                retained_profit.to_string(),
                gas_cost.to_string(),
                net_profit.to_string(),
                now_ms() as i64,
                finalized_block as i64,
            ],
        )?;
        txn.execute(
            "UPDATE nonce_reservations SET status = 'finalized', updated_at_ms = ?2
             WHERE bundle_id = ?1",
            params![bundle.bundle_id, now_ms() as i64],
        )?;
        txn.execute(
            "UPDATE bundles SET included = 1, included_block = ?2,
                    inclusion_checked_ms = ?3, inclusion_state = 'finalized_included',
                    observed_tx_hashes = ?4 WHERE id = ?1",
            params![
                bundle.bundle_id,
                block_number as i64,
                now_ms() as i64,
                serde_json::to_string(
                    &bundle
                        .tx_hashes
                        .iter()
                        .map(|hash| format!("{hash:?}"))
                        .collect::<Vec<_>>()
                )?
            ],
        )?;
        txn.commit()?;
        Ok(())
    }

    pub fn mark_bundle_not_included(&self, bundle_id: &str) -> Result<()> {
        let mut conn = self.conn.lock();
        let txn = conn.transaction()?;
        txn.execute(
            "UPDATE bundles SET included = 0, inclusion_checked_ms = ?2,
                    inclusion_state = 'finalized_not_included' WHERE id = ?1",
            params![bundle_id, now_ms() as i64],
        )?;
        txn.execute(
            "UPDATE nonce_reservations SET status = 'expired', updated_at_ms = ?2
             WHERE bundle_id = ?1",
            params![bundle_id, now_ms() as i64],
        )?;
        txn.commit()?;
        Ok(())
    }

    pub fn mark_bundle_observation(
        &self,
        bundle_id: &str,
        state: &str,
        observed_block: Option<u64>,
        observed_hashes: &[alloy_primitives::B256],
    ) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE bundles SET inclusion_state = ?2,
                    included_block = COALESCE(?3, included_block),
                    observed_tx_hashes = ?4, inclusion_checked_ms = ?5 WHERE id = ?1",
            params![
                bundle_id,
                state,
                observed_block.map(|value| value as i64),
                serde_json::to_string(
                    &observed_hashes
                        .iter()
                        .map(|hash| format!("{hash:?}"))
                        .collect::<Vec<_>>()
                )?,
                now_ms() as i64
            ],
        )?;
        Ok(())
    }

    pub fn finalize_bundle_state(
        &self,
        bundle_id: &str,
        state: &str,
        included: bool,
        included_block: Option<u64>,
        observed_hashes: &[alloy_primitives::B256],
    ) -> Result<()> {
        let mut conn = self.conn.lock();
        let txn = conn.transaction()?;
        txn.execute(
            "UPDATE bundles SET included = ?2, included_block = ?3,
                    inclusion_state = ?4, observed_tx_hashes = ?5,
                    inclusion_checked_ms = ?6 WHERE id = ?1",
            params![
                bundle_id,
                included as i32,
                included_block.map(|value| value as i64),
                state,
                serde_json::to_string(
                    &observed_hashes
                        .iter()
                        .map(|hash| format!("{hash:?}"))
                        .collect::<Vec<_>>()
                )?,
                now_ms() as i64
            ],
        )?;
        txn.execute(
            "UPDATE nonce_reservations SET status = ?2, updated_at_ms = ?3
             WHERE bundle_id = ?1",
            params![bundle_id, state, now_ms() as i64],
        )?;
        txn.commit()?;
        Ok(())
    }

    pub fn reserve_bundle_nonces(
        &self,
        bundle_id: &str,
        opportunity_id: &str,
        start_nonce: u64,
        nonce_count: u64,
        target_block: u64,
    ) -> Result<()> {
        self.conn.lock().execute(
            "INSERT OR REPLACE INTO nonce_reservations
             (bundle_id, opportunity_id, start_nonce, nonce_count, target_block,
              status, created_at_ms, updated_at_ms)
             VALUES (?1,?2,?3,?4,?5,'reserved',?6,?6)",
            params![
                bundle_id,
                opportunity_id,
                start_nonce as i64,
                nonce_count as i64,
                target_block as i64,
                now_ms() as i64
            ],
        )?;
        Ok(())
    }

    pub fn set_nonce_reservation_status(&self, bundle_id: &str, status: &str) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE nonce_reservations SET status = ?2, updated_at_ms = ?3
             WHERE bundle_id = ?1",
            params![bundle_id, status, now_ms() as i64],
        )?;
        Ok(())
    }

    pub fn active_nonce_reservations(&self) -> Result<Vec<ActiveNonceReservation>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT bundle_id, start_nonce, nonce_count, target_block
             FROM nonce_reservations
             WHERE status IN ('reserved','accepted','recovery_blocked')
             ORDER BY start_nonce ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ActiveNonceReservation {
                bundle_id: row.get(0)?,
                start_nonce: row.get::<_, i64>(1)?.max(0) as u64,
                nonce_count: row.get::<_, i64>(2)?.max(0) as u64,
                target_block: row.get::<_, i64>(3)?.max(0) as u64,
            })
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    pub fn record_relay_submission(
        &self,
        bundle_id: &str,
        opportunity_id: &str,
        relay: &str,
        accepted: bool,
        response: &serde_json::Value,
    ) -> Result<()> {
        self.conn.lock().execute(
            "INSERT OR REPLACE INTO relay_submissions
             (bundle_id, opportunity_id, relay, accepted, response, submitted_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                bundle_id,
                opportunity_id,
                relay,
                accepted as i32,
                response.to_string(),
                now_ms() as i64,
            ],
        )?;
        Ok(())
    }

    /// Decision-time opportunities that targeted this block and named a victim.
    /// Called before replay scoring starts, so post-mortem opportunities cannot
    /// be mistaken for observations that were actionable before inclusion.
    pub fn victim_opportunities_for_block(
        &self,
        block_number: u64,
    ) -> Result<Vec<OpportunityVictim>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, strategy, victims FROM opportunities
             WHERE target_block = ?1 AND victims != '' ORDER BY created_at_ms ASC",
        )?;
        let rows = stmt.query_map(params![block_number as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows.flatten() {
            for victim_hash in row.2.split(',').map(str::trim).filter(|v| !v.is_empty()) {
                out.push(OpportunityVictim {
                    opportunity_id: row.0.clone(),
                    strategy: row.1.clone(),
                    victim_hash: victim_hash.to_string(),
                });
            }
        }
        Ok(out)
    }

    pub fn atomic_arb_opportunities_for_block(
        &self,
        block_number: u64,
    ) -> Result<Vec<AtomicArbObservation>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, notes FROM opportunities
             WHERE target_block = ?1 AND strategy = 'atomic_arb'
             ORDER BY created_at_ms ASC",
        )?;
        let rows = stmt.query_map(params![block_number as i64], |row| {
            Ok(AtomicArbObservation {
                opportunity_id: row.get(0)?,
                notes: row.get(1)?,
            })
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    /// One included-block comparison: the fork's predicted victim-leg delta
    /// vs the victim's realised delta in the canonical block. The
    /// `Sequencer` qualification backend reads these as its independent
    /// second opinion (no relay exists on a sequencer chain).
    /// The fork's predicted victim-leg delta for an opportunity (the latest
    /// successful fork simulation that measured one), if any.
    pub fn victim_predicted_delta(&self, opportunity_id: &str) -> Option<i128> {
        self.conn
            .lock()
            .query_row(
                "SELECT victim_predicted_out_wei FROM simulations
             WHERE opportunity_id = ?1 AND backend = 'anvil_fork' AND success = 1
               AND victim_predicted_out_wei IS NOT NULL
             ORDER BY id DESC LIMIT 1",
                params![opportunity_id],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .and_then(|v| v.parse::<i128>().ok())
    }

    /// The bundle's raw signed transactions (from the durable payload), if
    /// the bundle is recorded. Raw-mode cancellation decodes nonces from
    /// these and hashes them for receipt checks.
    pub fn bundle_raw_txs(&self, bundle_id: &str) -> Option<Vec<Vec<u8>>> {
        let payload = self
            .conn
            .lock()
            .query_row(
                "SELECT payload FROM bundles WHERE id = ?1",
                params![bundle_id],
                |row| row.get::<_, String>(0),
            )
            .ok()?;
        let value: serde_json::Value = serde_json::from_str(&payload).ok()?;
        let txs = value.get("txs")?.as_array()?;
        let mut out = Vec::new();
        for t in txs {
            let s = t.as_str()?;
            out.push(hex::decode(s.strip_prefix("0x").unwrap_or(s)).ok()?);
        }
        Some(out)
    }

    pub fn record_block_comparison(
        &self,
        opportunity_id: &str,
        strategy: &str,
        victim_hash: &str,
        block_number: u64,
        predicted_wei: i128,
        realized_wei: i128,
    ) -> Result<()> {
        let error_bps = relative_error_bps(predicted_wei, realized_wei);
        self.conn.lock().execute(
            "INSERT INTO block_comparisons
             (opportunity_id, strategy, victim_hash, block_number,
              predicted_wei, realized_wei, error_bps, created_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                opportunity_id,
                strategy,
                victim_hash,
                block_number as i64,
                predicted_wei.to_string(),
                realized_wei.to_string(),
                error_bps as i64,
                crate::types::now_ms() as i64,
            ],
        )?;
        Ok(())
    }

    /// Independent state-fidelity sample for a victimless strategy.
    ///
    /// The unique key is `(opportunity, state, route, amount, direction)`.
    /// A replay of the same frame is `INSERT OR IGNORE` and does not inflate
    /// the qualification count. Reorgs flip `canonical` rather than deleting.
    #[allow(clippy::too_many_arguments)]
    pub fn record_state_comparison(
        &self,
        sample_id: &str,
        opportunity_id: &str,
        strategy: &str,
        source_state_id: &str,
        canonical_block: u64,
        canonical_hash: &str,
        route: &str,
        amount_in: &str,
        direction: &str,
        predicted_wei: i128,
        realized_wei: i128,
    ) -> Result<bool> {
        let error_bps = relative_error_bps(predicted_wei, realized_wei);
        let changed = self.conn.lock().execute(
            "INSERT OR IGNORE INTO state_comparisons
             (id, opportunity_id, strategy, source_state_id, canonical_block,
              canonical_hash, route, amount_in, direction,
              predicted_wei, realized_wei, error_bps, canonical, created_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,1,?13)",
            params![
                sample_id,
                opportunity_id,
                strategy,
                source_state_id,
                canonical_block as i64,
                canonical_hash,
                route,
                amount_in,
                direction,
                predicted_wei.to_string(),
                realized_wei.to_string(),
                error_bps as i64,
                now_ms() as i64,
            ],
        )?;
        Ok(changed == 1)
    }

    pub fn record_actual_mev_match(&self, matched: &ActualMevMatch) -> Result<()> {
        self.conn.lock().execute(
            "INSERT OR REPLACE INTO actual_mev_matches
             (opportunity_id, block_number, victim_hash, mev_tx_hashes, actor,
              gross_weth_wei, gas_cost_wei, net_weth_wei, confidence,
              confidence_score_bps, completeness, evidence, canonical, created_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,1,?13)",
            params![
                matched.opportunity_id,
                matched.block_number as i64,
                matched.victim_hash,
                serde_json::to_string(&matched.mev_tx_hashes)?,
                matched.actor,
                matched.gross_weth_wei.to_string(),
                matched.gas_cost_wei.to_string(),
                matched.net_weth_wei.to_string(),
                matched.confidence,
                matched.confidence_score_bps as i64,
                matched.completeness.to_string(),
                matched.evidence.to_string(),
                now_ms() as i64,
            ],
        )?;
        Ok(())
    }

    /// Load the durable kill-switch snapshot. Missing row → not tripped.
    pub fn load_risk_state(&self) -> Result<PersistedRiskState> {
        let conn = self.conn.lock();
        match conn.query_row(
            "SELECT kill_switch_tripped, tripped_at_ms, cumulative_net_wei
             FROM risk_state WHERE id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        ) {
            Ok((tripped, at, cum)) => Ok(PersistedRiskState {
                tripped: tripped != 0,
                tripped_at_ms: at.filter(|v| *v > 0).map(|v| v as u64),
                cumulative_net_wei: parse_i128_decimal(&cum),
            }),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(PersistedRiskState::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// How many smoke slots have already been consumed. Missing column/row → 0.
    pub fn smoke_used(&self) -> Result<u64> {
        let n: i64 = match self.conn.lock().query_row(
            "SELECT COALESCE(live_smoke_used, 0) FROM risk_state WHERE id = 1",
            [],
            |row| row.get(0),
        ) {
            Ok(n) => n,
            Err(rusqlite::Error::QueryReturnedNoRows) => 0,
            Err(e) => return Err(e.into()),
        };
        Ok(n.max(0) as u64)
    }

    /// Worst-case raw gas exposure reserved by smoke sends. This is sticky,
    /// like the count: a restart cannot refill the operator's risk budget.
    pub fn smoke_gas_at_risk_wei(&self) -> Result<U256> {
        let value: String = match self.conn.lock().query_row(
            "SELECT COALESCE(live_smoke_gas_risk_wei, '0') FROM risk_state WHERE id = 1",
            [],
            |row| row.get(0),
        ) {
            Ok(value) => value,
            Err(rusqlite::Error::QueryReturnedNoRows) => "0".into(),
            Err(error) => return Err(error.into()),
        };
        value
            .parse::<U256>()
            .with_context(|| "invalid durable live_smoke_gas_risk_wei")
    }

    /// Atomically consume one smoke slot and, for raw mode, reserve the
    /// transaction's worst-case gas exposure. `max_gas_cost_wei=None` is the
    /// relay-bundle path (count only). A missing/zero raw budget fails closed.
    pub fn try_consume_smoke_budget(
        &self,
        max_count: u64,
        max_gas_cost_wei: Option<U256>,
        gas_at_risk_wei: U256,
    ) -> Result<bool> {
        if max_count == 0 {
            return Ok(false);
        }
        let conn = self.conn.lock();
        let (used, used_gas): (i64, String) = conn.query_row(
            "SELECT COALESCE(live_smoke_used, 0),
                    COALESCE(live_smoke_gas_risk_wei, '0')
             FROM risk_state WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if used.max(0) as u64 >= max_count {
            return Ok(false);
        }
        let current_gas = used_gas
            .parse::<U256>()
            .with_context(|| "invalid durable live_smoke_gas_risk_wei")?;
        let next_gas = current_gas.saturating_add(gas_at_risk_wei);
        if let Some(max_gas) = max_gas_cost_wei {
            if max_gas.is_zero() || next_gas > max_gas {
                return Ok(false);
            }
        }
        let changed = conn.execute(
            "UPDATE risk_state
             SET live_smoke_used = live_smoke_used + 1,
                 live_smoke_gas_risk_wei = ?1
             WHERE id = 1 AND live_smoke_used = ?2",
            params![next_gas.to_string(), used],
        )?;
        Ok(changed == 1)
    }

    /// Count-only compatibility wrapper used by relay-bundle smoke and tests.
    pub fn try_consume_smoke_slot(&self, max: u64) -> Result<bool> {
        self.try_consume_smoke_budget(max, None, U256::ZERO)
    }

    /// Persist the kill-switch flag and the cumulative that produced it.
    ///
    /// Synchronous on purpose: this is safety state, not telemetry. A full
    /// `AsyncStore` queue must not be allowed to drop a trip. `tripped_at_ms`
    /// is sticky for the life of a trip so a later persist of the same trip
    /// does not rewrite the original timestamp. The update list does not
    /// touch `live_smoke_used`, so a trip cannot refill or wipe the budget.
    pub fn persist_kill_switch(&self, tripped: bool, cumulative_net_wei: i128) -> Result<()> {
        let conn = self.conn.lock();
        let existing_at: Option<i64> = conn
            .query_row(
                "SELECT tripped_at_ms FROM risk_state WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .ok()
            .flatten();
        let tripped_at = if tripped {
            Some(existing_at.filter(|v| *v > 0).unwrap_or(now_ms() as i64))
        } else {
            None
        };
        conn.execute(
            "INSERT INTO risk_state (id, kill_switch_tripped, tripped_at_ms, cumulative_net_wei)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
               kill_switch_tripped = excluded.kill_switch_tripped,
               tripped_at_ms = excluded.tripped_at_ms,
               cumulative_net_wei = excluded.cumulative_net_wei",
            params![tripped as i32, tripped_at, cumulative_net_wei.to_string()],
        )?;
        Ok(())
    }

    pub fn record_qualification_incident(
        &self,
        kind: &str,
        detail: &str,
        occurred_at_ms: u64,
    ) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO qualification_incidents (kind, detail, occurred_at_ms)
             VALUES (?1,?2,?3)",
            params![kind, detail, occurred_at_ms as i64],
        )?;
        Ok(())
    }

    pub fn qualification_incident_count(&self, since_ms: u64) -> Result<u64> {
        let count: i64 = self.conn.lock().query_row(
            "SELECT COUNT(*) FROM qualification_incidents WHERE occurred_at_ms >= ?1",
            params![since_ms as i64],
            |row| row.get(0),
        )?;
        Ok(count.max(0) as u64)
    }

    /// Canonical observation continuity over the requested qualification window.
    /// A prior observation at or before `since_ms` anchors the left edge; the
    /// right edge includes the gap from the newest observation to `now_ms`.
    pub fn observation_coverage(&self, since_ms: u64, now_ms: u64) -> Result<ObservationCoverage> {
        let conn = self.conn.lock();
        let first_seen_ms = conn
            .query_row(
                "SELECT MIN(seen_at_ms) FROM blocks WHERE canonical = 1",
                [],
                |row| row.get::<_, Option<i64>>(0),
            )?
            .map(|value| value.max(0) as u64);
        let anchor = conn
            .query_row(
                "SELECT MAX(seen_at_ms) FROM blocks
                 WHERE canonical = 1 AND seen_at_ms <= ?1",
                params![since_ms as i64],
                |row| row.get::<_, Option<i64>>(0),
            )?
            .map(|value| value.max(0) as u64);
        let mut stmt = conn.prepare(
            "SELECT seen_at_ms FROM blocks
             WHERE canonical = 1 AND seen_at_ms > ?1
             ORDER BY seen_at_ms ASC",
        )?;
        let rows = stmt.query_map(params![since_ms as i64], |row| row.get::<_, i64>(0))?;
        let mut previous = anchor;
        let mut maximum_gap_ms = if anchor.is_some() { 0 } else { u64::MAX };
        let mut last_seen_ms = anchor;
        let mut observations = 0u64;
        for seen in rows.flatten().map(|value| value.max(0) as u64) {
            if let Some(prior) = previous {
                maximum_gap_ms = maximum_gap_ms.max(seen.saturating_sub(prior));
            }
            previous = Some(seen);
            last_seen_ms = Some(seen);
            observations = observations.saturating_add(1);
        }
        if let Some(last) = last_seen_ms {
            maximum_gap_ms = maximum_gap_ms.max(now_ms.saturating_sub(last));
        }
        Ok(ObservationCoverage {
            first_seen_ms,
            last_seen_ms,
            maximum_gap_ms,
            observations,
        })
    }

    /// Strategy-specific fork, relay and corresponding-chain evidence. The
    /// comparison vectors contain exact relative errors in basis points.
    /// Per-strategy qualification evidence.
    ///
    /// `backend` selects the source of the *independent second opinion*
    /// (the `relay_errors_bps` field, a name kept for API stability):
    /// - `Relay` (mainnet): fork net vs relay `eth_callBundle` net.
    /// - `Sequencer` (Base et al., no relay market): fork prediction vs an
    ///   independently recorded canonical state transition in
    ///   `block_comparisons`. High-confidence route matches remain in the
    ///   separate corresponding-chain population below; they are never reused
    ///   as the second opinion. Victimless strategies therefore remain
    ///   unqualified until they record a genuine state-comparison row.
    pub fn qualification_evidence(
        &self,
        since_ms: u64,
        strategy: Strategy,
        minimum_confidence_bps: u64,
        backend: crate::config::QualificationBackend,
    ) -> Result<QualificationEvidence> {
        let conn = self.conn.lock();
        let strategy = strategy.as_str();
        let fork_samples: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT opportunity_id) FROM simulations
             WHERE backend = 'anvil_fork' AND success = 1 AND created_at_ms >= ?1
               AND strategy = ?2 AND COALESCE(reorged, 0) = 0",
            params![since_ms as i64, strategy],
            |row| row.get(0),
        )?;

        let relay_errors_bps = match backend {
            crate::config::QualificationBackend::Relay => {
                let mut relay_stmt = conn.prepare(
                    "SELECT CAST(f.net_wei AS TEXT), CAST(r.net_wei AS TEXT)
                     FROM simulations f
                     JOIN simulations r ON r.opportunity_id = f.opportunity_id
                     WHERE f.backend = 'anvil_fork' AND r.backend = 'relay_call_bundle'
                       AND f.success = 1 AND r.success = 1
                       AND f.strategy = ?1 AND f.created_at_ms >= ?2
                       AND COALESCE(f.reorged, 0) = 0 AND COALESCE(r.reorged, 0) = 0
                       AND f.id = (SELECT MAX(f2.id) FROM simulations f2
                                   WHERE f2.opportunity_id = f.opportunity_id
                                     AND f2.backend = 'anvil_fork')
                       AND r.id = (SELECT MAX(r2.id) FROM simulations r2
                                   WHERE r2.opportunity_id = r.opportunity_id
                                     AND r2.backend = 'relay_call_bundle')",
                )?;
                let relay_rows = relay_stmt
                    .query_map(params![strategy, since_ms as i64], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?;
                relay_rows
                    .flatten()
                    .map(|(fork, relay)| {
                        relative_error_bps(parse_i128_decimal(&fork), parse_i128_decimal(&relay))
                    })
                    .collect()
            }
            crate::config::QualificationBackend::Sequencer => {
                // (a) Victim-replay fidelity: the fork's predicted victim-leg
                // delta vs the victim's realised delta in the canonical
                // block — the included-block second opinion.
                // (b) Victimless independent samples (`state_comparisons`):
                // one (opportunity, state, route, amount, direction) row.
                // Do not append `actual_mev_matches` here. Those rows feed
                // the separate corresponding-chain population below;
                // counting one row in both populations would manufacture
                // independence.
                let mut errs: Vec<u64> = Vec::new();
                {
                    let mut block_stmt = conn.prepare(
                        "SELECT error_bps FROM block_comparisons
                         WHERE strategy = ?1 AND created_at_ms >= ?2",
                    )?;
                    errs.extend(
                        block_stmt
                            .query_map(params![strategy, since_ms as i64], |row| {
                                row.get::<_, i64>(0)
                            })?
                            .flatten()
                            .map(|e| e.max(0) as u64),
                    );
                }
                {
                    let mut state_stmt = conn.prepare(
                        "SELECT error_bps FROM state_comparisons
                         WHERE strategy = ?1 AND created_at_ms >= ?2 AND canonical = 1",
                    )?;
                    errs.extend(
                        state_stmt
                            .query_map(params![strategy, since_ms as i64], |row| {
                                row.get::<_, i64>(0)
                            })?
                            .flatten()
                            .map(|e| e.max(0) as u64),
                    );
                }
                errs
            }
        };

        let mut actual_errors_bps = Vec::new();
        let mut actual_stmt = conn.prepare(
            "SELECT CAST(f.net_wei AS TEXT), CAST(a.net_weth_wei AS TEXT)
             FROM actual_mev_matches a
             JOIN opportunities o ON o.id = a.opportunity_id
             JOIN simulations f ON f.opportunity_id = a.opportunity_id
             WHERE o.strategy = ?1 AND a.canonical = 1
               AND a.created_at_ms >= ?2 AND a.confidence_score_bps >= ?3
               AND f.backend = 'anvil_fork' AND f.success = 1
               AND COALESCE(f.reorged, 0) = 0
               AND f.id = (SELECT MAX(f2.id) FROM simulations f2
                           WHERE f2.opportunity_id = f.opportunity_id
                             AND f2.backend = 'anvil_fork')",
        )?;
        let actual_rows = actual_stmt.query_map(
            params![strategy, since_ms as i64, minimum_confidence_bps as i64],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?;
        actual_errors_bps.extend(actual_rows.flatten().map(|(fork, actual)| {
            relative_error_bps(parse_i128_decimal(&fork), parse_i128_decimal(&actual))
        }));

        // Once live execution has occurred, exact finalized own outcomes are
        // also valid corresponding-chain evidence (confidence 10_000).
        let mut own_stmt = conn.prepare(
            "SELECT CAST(f.net_wei AS TEXT), CAST(e.net_profit_wei AS TEXT)
             FROM execution_outcomes e
             JOIN opportunities o ON o.id = e.opportunity_id
             JOIN simulations f ON f.opportunity_id = e.opportunity_id
             WHERE o.strategy = ?1 AND e.canonical = 1
               AND e.created_at_ms >= ?2 AND e.finalized_block IS NOT NULL
               AND f.backend = 'anvil_fork' AND f.success = 1
               AND COALESCE(f.reorged, 0) = 0
               AND f.id = (SELECT MAX(f2.id) FROM simulations f2
                           WHERE f2.opportunity_id = f.opportunity_id
                             AND f2.backend = 'anvil_fork')",
        )?;
        let own_rows = own_stmt.query_map(params![strategy, since_ms as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        actual_errors_bps.extend(own_rows.flatten().map(|(fork, actual)| {
            relative_error_bps(parse_i128_decimal(&fork), parse_i128_decimal(&actual))
        }));

        Ok(QualificationEvidence {
            fork_samples: fork_samples.max(0) as u64,
            relay_errors_bps,
            actual_errors_bps,
        })
    }

    pub fn actual_mev_summary(&self) -> Result<serde_json::Value> {
        let conn = self.conn.lock();
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM actual_mev_matches WHERE canonical = 1",
            [],
            |row| row.get(0),
        )?;
        let high: i64 = conn.query_row(
            "SELECT COUNT(*) FROM actual_mev_matches WHERE canonical = 1 AND confidence = 'high'",
            [],
            |row| row.get(0),
        )?;
        Ok(serde_json::json!({"matches": total, "highConfidence": high}))
    }

    pub fn recent_actual_mev_matches(&self, limit: i64) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT opportunity_id, block_number, victim_hash, mev_tx_hashes, actor,
                    gross_weth_wei, gas_cost_wei, CAST(net_weth_wei AS TEXT),
                    confidence, confidence_score_bps, completeness, evidence, created_at_ms
             FROM actual_mev_matches WHERE canonical = 1
             ORDER BY block_number DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            let hashes: String = row.get(3)?;
            let completeness: String = row.get(10)?;
            let evidence: String = row.get(11)?;
            Ok(serde_json::json!({
                "opportunityId": row.get::<_, String>(0)?,
                "blockNumber": row.get::<_, i64>(1)?,
                "victimHash": row.get::<_, String>(2)?,
                "mevTxHashes": serde_json::from_str::<serde_json::Value>(&hashes).unwrap_or_default(),
                "actor": row.get::<_, Option<String>>(4)?,
                "grossWethWei": row.get::<_, String>(5)?,
                "gasCostWei": row.get::<_, String>(6)?,
                "netWethWei": row.get::<_, String>(7)?,
                "confidence": row.get::<_, String>(8)?,
                "confidenceScoreBps": row.get::<_, i64>(9)?,
                "completeness": serde_json::from_str::<serde_json::Value>(&completeness).unwrap_or_default(),
                "evidence": serde_json::from_str::<serde_json::Value>(&evidence).unwrap_or_default(),
                "createdAtMs": row.get::<_, i64>(12)?,
            }))
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    pub fn recent_execution_outcomes(&self, limit: i64) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT b.id, b.opportunity_id, b.strategy, b.target_block,
                    b.inclusion_state, b.included, b.included_block,
                    b.observed_tx_hashes, b.created_at_ms,
                    e.tx_hashes, e.gross_profit_wei, e.bribe_wei,
                    e.retained_profit_wei, e.gas_cost_wei,
                    CAST(e.net_profit_wei AS TEXT), e.canonical, e.finalized_block,
                    e.created_at_ms
             FROM bundles b
             LEFT JOIN execution_outcomes e ON e.bundle_id = b.id
             WHERE b.submitted = 1
             ORDER BY b.created_at_ms DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            let observed = row.get::<_, Option<String>>(7)?.unwrap_or_else(|| "[]".into());
            let exact_hashes = row.get::<_, Option<String>>(9)?.unwrap_or_else(|| "[]".into());
            Ok(serde_json::json!({
                "bundleId": row.get::<_, String>(0)?,
                "opportunityId": row.get::<_, String>(1)?,
                "strategy": row.get::<_, String>(2)?,
                "targetBlock": row.get::<_, i64>(3)?,
                "state": row.get::<_, String>(4)?,
                "included": row.get::<_, Option<i64>>(5)?.map(|value| value == 1),
                "includedBlock": row.get::<_, Option<i64>>(6)?,
                "observedTxHashes": serde_json::from_str::<serde_json::Value>(&observed).unwrap_or_default(),
                "submittedAtMs": row.get::<_, i64>(8)?,
                "txHashes": serde_json::from_str::<serde_json::Value>(&exact_hashes).unwrap_or_default(),
                "grossProfitWei": row.get::<_, Option<String>>(10)?,
                "builderPaymentWei": row.get::<_, Option<String>>(11)?,
                "retainedProfitWei": row.get::<_, Option<String>>(12)?,
                "gasCostWei": row.get::<_, Option<String>>(13)?,
                "netProfitWei": row.get::<_, Option<String>>(14)?,
                "canonical": row.get::<_, Option<i64>>(15)?.map(|value| value == 1),
                "finalizedBlock": row.get::<_, Option<i64>>(16)?,
                "reconciledAtMs": row.get::<_, Option<i64>>(17)?,
            }))
        })?;
        Ok(rows.filter_map(Result::ok).collect())
    }

    /// Record a delivered block (deduplicated per relay + slot).
    pub fn record_relay_block(&self, b: &crate::types::RelayBlock) -> Result<()> {
        self.conn.lock().execute(
            "INSERT OR IGNORE INTO relay_blocks
             (relay, slot, block_number, block_hash, builder, value_wei, gas_used, num_tx, seen_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                b.relay,
                b.slot as i64,
                b.block_number as i64,
                format!("{:?}", b.block_hash),
                b.builder,
                b.value_wei.to_string(),
                b.gas_used as i64,
                b.num_tx as i64,
                crate::types::now_ms() as i64,
            ],
        )?;
        Ok(())
    }

    /// Record one transaction inside a delivered block. `index` is the
    /// transaction's position within the block, preserved for replay ordering.
    pub fn record_relay_block_tx(
        &self,
        b: &crate::types::RelayBlock,
        tx: &crate::types::PendingTx,
        index: usize,
    ) -> Result<()> {
        self.conn.lock().execute(
            "INSERT OR IGNORE INTO relay_block_txs
             (block_number, tx_index, hash, from_addr, to_addr, value_wei, nonce, gas, selector, input)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                b.block_number as i64,
                index as i64,
                format!("{:?}", tx.hash),
                tx.from.map(|a| format!("{a:?}")),
                tx.to.map(|a| format!("{a:?}")),
                tx.value.to_string(),
                tx.nonce as i64,
                tx.gas as i64,
                tx.selector().map(|s| format!("0x{}", hex::encode(s))),
                format!("0x{}", hex::encode(&tx.input)),
            ],
        )?;
        Ok(())
    }

    /// Record a delivered block together with all of its transactions in one
    /// transaction.
    ///
    /// A mainnet block carries ~150–200 transactions. Inserting them one at a
    /// time meant ~200 separate implicit transactions — 200 commits, 200
    /// mutex acquisitions — every 12 seconds, on the task that also has to
    /// score the block. One explicit transaction turns that into a single
    /// commit, and reuses one prepared statement for every row.
    pub fn record_relay_block_with_txs(
        &self,
        b: &crate::types::RelayBlock,
        txs: &[crate::types::PendingTx],
    ) -> Result<()> {
        let mut conn = self.conn.lock();
        let txn = conn.transaction()?;
        txn.execute(
            "INSERT OR IGNORE INTO relay_blocks
             (relay, slot, block_number, block_hash, builder, value_wei, gas_used, num_tx, seen_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                b.relay,
                b.slot as i64,
                b.block_number as i64,
                format!("{:?}", b.block_hash),
                b.builder,
                b.value_wei.to_string(),
                b.gas_used as i64,
                b.num_tx as i64,
                crate::types::now_ms() as i64,
            ],
        )?;
        {
            // Prepared once, executed per row.
            let mut stmt = txn.prepare_cached(
                "INSERT OR IGNORE INTO relay_block_txs
                 (block_number, tx_index, hash, from_addr, to_addr, value_wei, nonce, gas, selector, input)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            )?;
            for (index, tx) in txs.iter().enumerate() {
                stmt.execute(params![
                    b.block_number as i64,
                    index as i64,
                    format!("{:?}", tx.hash),
                    tx.from.map(|a| format!("{a:?}")),
                    tx.to.map(|a| format!("{a:?}")),
                    tx.value.to_string(),
                    tx.nonce as i64,
                    tx.gas as i64,
                    tx.selector().map(|s| format!("0x{}", hex::encode(s))),
                    format!("0x{}", hex::encode(&tx.input)),
                ])?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// PnL per strategy, computed with integer arithmetic over decimal
    /// strings. SQLite `REAL` loses wei precision and signed 64-bit integers
    /// saturate above ~9.22 ETH.
    pub fn pnl(&self) -> Result<Vec<PnlSummary>> {
        #[derive(Default)]
        struct Aggregate {
            simulations: i64,
            wins: i64,
            losses: i64,
            gross: U256,
            gas: U256,
            net: i128,
            best: Option<i128>,
            worst: Option<i128>,
            latency_sum: u128,
        }
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT strategy, CAST(gross_wei AS TEXT), CAST(gas_cost_wei AS TEXT),
                    CAST(net_wei AS TEXT), latency_ms
             FROM simulations
             WHERE backend = 'anvil_fork' AND COALESCE(reorged, 0) = 0",
        )?;
        let mut rows = stmt.query([])?;
        let mut by_strategy: std::collections::BTreeMap<String, Aggregate> =
            std::collections::BTreeMap::new();
        while let Some(row) = rows.next()? {
            let strategy: String = row.get(0)?;
            let gross = parse_u256_decimal(&row.get::<_, String>(1)?);
            let gas = parse_u256_decimal(&row.get::<_, String>(2)?);
            let net = parse_i128_decimal(&row.get::<_, String>(3)?);
            let latency = row.get::<_, i64>(4)?.max(0) as u128;
            let aggregate = by_strategy.entry(strategy).or_default();
            aggregate.simulations += 1;
            if net > 0 {
                aggregate.wins += 1;
            } else {
                aggregate.losses += 1;
            }
            aggregate.gross = aggregate.gross.saturating_add(gross);
            aggregate.gas = aggregate.gas.saturating_add(gas);
            aggregate.net = aggregate.net.saturating_add(net);
            aggregate.best = Some(aggregate.best.map_or(net, |value| value.max(net)));
            aggregate.worst = Some(aggregate.worst.map_or(net, |value| value.min(net)));
            aggregate.latency_sum = aggregate.latency_sum.saturating_add(latency);
        }
        Ok(by_strategy
            .into_iter()
            .map(|(strategy, aggregate)| PnlSummary {
                strategy,
                simulations: aggregate.simulations,
                wins: aggregate.wins,
                losses: aggregate.losses,
                gross_profit_wei: aggregate.gross.to_string(),
                gas_spent_wei: aggregate.gas.to_string(),
                net_profit_wei: aggregate.net.to_string(),
                best_net_wei: aggregate.best.unwrap_or(0).to_string(),
                worst_net_wei: aggregate.worst.unwrap_or(0).to_string(),
                avg_latency_ms: if aggregate.simulations == 0 {
                    0.0
                } else {
                    aggregate.latency_sum as f64 / aggregate.simulations as f64
                },
            })
            .collect())
    }

    /// Cumulative net PnL in exact signed wei across every fork simulation.
    pub fn cumulative_net(&self) -> Result<i128> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT CAST(net_wei AS TEXT) FROM simulations
             WHERE backend = 'anvil_fork' AND COALESCE(reorged, 0) = 0",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        Ok(rows.filter_map(Result::ok).fold(0i128, |sum, value| {
            sum.saturating_add(parse_i128_decimal(&value))
        }))
    }

    pub fn recent_simulations(
        &self,
        limit: i64,
        strategy: Option<Strategy>,
    ) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock();
        let sql = "SELECT s.opportunity_id, s.strategy, s.backend, s.success, s.gross_wei, s.gas_used,
                          s.gas_cost_wei, s.bribe_wei, CAST(s.net_wei AS TEXT), s.revert_reason, s.target_block,
                          s.latency_ms, s.created_at_ms, COALESCE(o.notes, ''), COALESCE(o.victims, '')
                   FROM simulations s
                   LEFT JOIN opportunities o ON o.id = s.opportunity_id
                   WHERE (?1 IS NULL OR s.strategy = ?1)
                   ORDER BY s.id DESC LIMIT ?2";
        let mut stmt = conn.prepare(sql)?;
        let strat = strategy.map(|s| s.as_str().to_string());
        let rows = stmt.query_map(params![strat, limit], |row| {
            Ok(serde_json::json!({
                "opportunityId": row.get::<_, String>(0)?,
                "strategy": row.get::<_, String>(1)?,
                "backend": row.get::<_, String>(2)?,
                "success": row.get::<_, i64>(3)? == 1,
                "grossWei": row.get::<_, String>(4)?,
                "gasUsed": row.get::<_, i64>(5)?,
                "gasCostWei": row.get::<_, String>(6)?,
                "bribeWei": row.get::<_, String>(7)?,
                "netWei": row.get::<_, String>(8)?,
                "revertReason": row.get::<_, Option<String>>(9)?,
                "targetBlock": row.get::<_, i64>(10)?,
                "latencyMs": row.get::<_, i64>(11)?,
                "createdAtMs": row.get::<_, i64>(12)?,
                "notes": row.get::<_, String>(13)?,
                // Comma-separated victim tx hashes from the parent opportunity.
                // The dashboard links each simulation to the transaction it
                // reacted to on the block explorer. Empty when the
                // opportunity is gone (older rows, strategy-less sims).
                "victims": row.get::<_, String>(14)?,
            }))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn recent_opportunities(&self, limit: i64) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, strategy, target_block, profit_token, expected_wei, notional_wei, victims, notes, created_at_ms
             FROM opportunities ORDER BY created_at_ms DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "strategy": row.get::<_, String>(1)?,
                "targetBlock": row.get::<_, i64>(2)?,
                "profitToken": row.get::<_, String>(3)?,
                "expectedWei": row.get::<_, String>(4)?,
                "notionalWei": row.get::<_, String>(5)?,
                "victims": row.get::<_, String>(6)?,
                "notes": row.get::<_, String>(7)?,
                "createdAtMs": row.get::<_, i64>(8)?,
            }))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Net PnL bucketed by block, preserving signed integer precision.
    pub fn pnl_series(&self, limit: i64) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT target_block, CAST(net_wei AS TEXT)
             FROM simulations WHERE backend = 'anvil_fork' AND COALESCE(reorged, 0) = 0
             ORDER BY target_block ASC, id ASC",
        )?;
        let mut rows = stmt.query([])?;
        let mut blocks: std::collections::BTreeMap<i64, (i128, i64)> =
            std::collections::BTreeMap::new();
        while let Some(row) = rows.next()? {
            let block: i64 = row.get(0)?;
            let net = parse_i128_decimal(&row.get::<_, String>(1)?);
            let entry = blocks.entry(block).or_insert((0, 0));
            entry.0 = entry.0.saturating_add(net);
            entry.1 += 1;
        }
        let keep = limit.max(0) as usize;
        let skip = blocks.len().saturating_sub(keep);
        Ok(blocks
            .into_iter()
            .skip(skip)
            .map(|(block, (net, count))| {
                serde_json::json!({"block": block, "netWei": net.to_string(), "count": count})
            })
            .collect())
    }

    pub fn recent_relay_bids(&self, limit: i64) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT relay, slot, builder, value_wei, seen_at_ms FROM relay_bids ORDER BY slot DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok(serde_json::json!({
                "relay": row.get::<_, String>(0)?,
                "slot": row.get::<_, i64>(1)?,
                "builder": row.get::<_, String>(2)?,
                "valueWei": row.get::<_, String>(3)?,
                "seenAtMs": row.get::<_, i64>(4)?,
            }))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Delivered blocks ingested from the bloXroute Max Profit relay, newest first.
    pub fn recent_relay_blocks(&self, limit: i64) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT relay, slot, block_number, block_hash, builder, value_wei, gas_used, num_tx, seen_at_ms
             FROM relay_blocks ORDER BY block_number DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok(serde_json::json!({
                "relay": row.get::<_, String>(0)?,
                "slot": row.get::<_, i64>(1)?,
                "blockNumber": row.get::<_, i64>(2)?,
                "blockHash": row.get::<_, String>(3)?,
                "builder": row.get::<_, String>(4)?,
                "valueWei": row.get::<_, String>(5)?,
                "gasUsed": row.get::<_, i64>(6)?,
                "numTx": row.get::<_, i64>(7)?,
                "seenAtMs": row.get::<_, i64>(8)?,
            }))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Transactions stored for delivered blocks. When `block_number` is given,
    /// only that block's transactions are returned; otherwise the newest across
    /// all blocks are returned.
    pub fn relay_block_txs(
        &self,
        block_number: Option<u64>,
        limit: i64,
    ) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT block_number, tx_index, hash, from_addr, to_addr, value_wei, nonce, gas, selector, input
             FROM relay_block_txs
             WHERE (?1 IS NULL OR block_number = ?1)
             ORDER BY block_number DESC, tx_index ASC LIMIT ?2",
        )?;
        let bn = block_number.map(|n| n as i64);
        let rows = stmt.query_map(params![bn, limit], |row| {
            Ok(serde_json::json!({
                "blockNumber": row.get::<_, i64>(0)?,
                "txIndex": row.get::<_, i64>(1)?,
                "hash": row.get::<_, String>(2)?,
                "from": row.get::<_, Option<String>>(3)?,
                "to": row.get::<_, Option<String>>(4)?,
                "valueWei": row.get::<_, String>(5)?,
                "nonce": row.get::<_, i64>(6)?,
                "gas": row.get::<_, i64>(7)?,
                "selector": row.get::<_, Option<String>>(8)?,
                "input": row.get::<_, String>(9)?,
            }))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }
}

// ---------------------------------------------------------------------------
// Directional sniper lane persistence
//
// A separate `impl` block on purpose: everything the sniper stores lives here,
// touches only the three `sniper_*` tables, and can be lifted out whole. No
// method in this block is called from the atomic path.
// ---------------------------------------------------------------------------

impl Store {
    /// Write (or overwrite) a position. Called before the entry is submitted
    /// and after every state change, so a crash can never leave open exposure
    /// that the bot does not know about on restart.
    pub fn upsert_sniper_position(&self, p: &crate::sniper::Position) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO sniper_positions
             (id, chain_id, token, pair, venue, state, trigger_tx, entry_tx,
              entry_cost_wei, entry_qty, remaining_qty, realized_wei, gas_spent_wei,
              peak_value_wei, opened_block, opened_at_ms, closed_at_ms, exit_reason,
              entry_verdict, notes, execution_mode, settlement, tx_status, exit_tx)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24)
             ON CONFLICT(id) DO UPDATE SET
                state          = excluded.state,
                entry_tx       = excluded.entry_tx,
                exit_tx        = excluded.exit_tx,
                entry_cost_wei = excluded.entry_cost_wei,
                entry_qty      = excluded.entry_qty,
                remaining_qty  = excluded.remaining_qty,
                realized_wei   = excluded.realized_wei,
                gas_spent_wei  = excluded.gas_spent_wei,
                peak_value_wei = excluded.peak_value_wei,
                closed_at_ms   = excluded.closed_at_ms,
                exit_reason    = excluded.exit_reason,
                notes          = excluded.notes,
                execution_mode = excluded.execution_mode,
                settlement     = excluded.settlement,
                tx_status      = excluded.tx_status",
            params![
                p.id,
                p.chain_id as i64,
                format!("{:?}", p.token),
                format!("{:?}", p.pair),
                p.venue,
                p.state.as_str(),
                p.trigger_tx.map(|h| format!("{h:?}")),
                p.entry_tx.map(|h| format!("{h:?}")),
                p.entry_cost_wei.to_string(),
                p.entry_qty.to_string(),
                p.remaining_qty.to_string(),
                p.realized_wei.to_string(),
                p.gas_spent_wei.to_string(),
                p.peak_value_wei.to_string(),
                p.opened_block as i64,
                p.opened_at_ms as i64,
                p.closed_at_ms.map(|v| v as i64),
                p.exit_reason.map(|r| r.as_str()),
                p.entry_verdict,
                p.notes,
                p.execution_mode.as_str(),
                p.settlement.as_str(),
                p.tx_status.as_str(),
                p.exit_tx.map(|h| format!("{h:?}")),
            ],
        )?;
        Ok(())
    }

    /// Every position still holding (or awaiting) exposure. This is what the
    /// lane hydrates from at boot.
    pub fn live_sniper_positions(&self) -> Result<Vec<crate::sniper::Position>> {
        self.sniper_positions_where("state IN ('pending','open','scaling')", 4_096)
    }

    /// Most recent positions regardless of state, newest first.
    pub fn recent_sniper_positions(&self, limit: usize) -> Result<Vec<crate::sniper::Position>> {
        self.sniper_positions_where("1=1", limit)
    }

    fn sniper_positions_where(
        &self,
        predicate: &str,
        limit: usize,
    ) -> Result<Vec<crate::sniper::Position>> {
        use crate::sniper::{ExitReason, Position, PositionState};

        let sql = format!(
            "SELECT id, chain_id, token, pair, venue, state, trigger_tx, entry_tx,
                    entry_cost_wei, entry_qty, remaining_qty, realized_wei, gas_spent_wei,
                    peak_value_wei, opened_block, opened_at_ms, closed_at_ms, exit_reason,
                    entry_verdict, notes, execution_mode, settlement, tx_status
                    , exit_tx
             FROM sniper_positions WHERE {predicate}
             ORDER BY opened_at_ms DESC LIMIT {limit}"
        );
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| {
            let state = match row.get::<_, String>(5)?.as_str() {
                "pending" => PositionState::Pending,
                "open" => PositionState::Open,
                "scaling" => PositionState::Scaling,
                "abandoned" => PositionState::Abandoned,
                _ => PositionState::Closed,
            };
            let exit_reason = row.get::<_, Option<String>>(17)?.and_then(|r| {
                Some(match r.as_str() {
                    "take_profit_pct" => ExitReason::TakeProfitPct,
                    "take_profit_abs" => ExitReason::TakeProfitAbs,
                    "stop_loss" => ExitReason::StopLoss,
                    "trailing_stop" => ExitReason::TrailingStop,
                    "max_hold" => ExitReason::MaxHold,
                    "honeypot_detected" => ExitReason::HoneypotDetected,
                    "manual" => ExitReason::Manual,
                    "risk_stop" => ExitReason::RiskStop,
                    _ => return None,
                })
            });
            Ok(Position {
                id: row.get(0)?,
                chain_id: row.get::<_, i64>(1)? as u64,
                token: parse_address(&row.get::<_, String>(2)?),
                pair: parse_address(&row.get::<_, String>(3)?),
                venue: row.get(4)?,
                state,
                trigger_tx: row
                    .get::<_, Option<String>>(6)?
                    .and_then(|h| parse_b256(&h)),
                entry_tx: row
                    .get::<_, Option<String>>(7)?
                    .and_then(|h| parse_b256(&h)),
                entry_cost_wei: parse_u256_decimal(&row.get::<_, String>(8)?),
                entry_qty: parse_u256_decimal(&row.get::<_, String>(9)?),
                remaining_qty: parse_u256_decimal(&row.get::<_, String>(10)?),
                realized_wei: parse_u256_decimal(&row.get::<_, String>(11)?),
                gas_spent_wei: parse_u256_decimal(&row.get::<_, String>(12)?),
                peak_value_wei: parse_u256_decimal(&row.get::<_, String>(13)?),
                opened_block: row.get::<_, i64>(14)? as u64,
                opened_at_ms: row.get::<_, i64>(15)? as u64,
                closed_at_ms: row.get::<_, Option<i64>>(16)?.map(|v| v as u64),
                exit_reason,
                entry_verdict: row.get(18)?,
                notes: row.get(19)?,
                execution_mode: crate::sniper::ExecutionMode::parse(&row.get::<_, String>(20)?),
                settlement: crate::sniper::Settlement::parse(&row.get::<_, String>(21)?),
                tx_status: crate::sniper::TxStatus::parse(&row.get::<_, String>(22)?),
                exit_tx: row
                    .get::<_, Option<String>>(23)?
                    .and_then(|h| parse_b256(&h)),
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Append an entry or exit fill.
    #[allow(clippy::too_many_arguments)]
    pub fn record_sniper_fill(
        &self,
        id: &str,
        position_id: &str,
        side: &str,
        reason: &str,
        qty: U256,
        weth_wei: U256,
        gas_wei: U256,
        tx_hash: Option<String>,
        block_number: Option<u64>,
        execution_mode: crate::sniper::ExecutionMode,
    ) -> Result<()> {
        self.conn.lock().execute(
            "INSERT OR IGNORE INTO sniper_fills
             (id, position_id, side, reason, qty, weth_wei, gas_wei, tx_hash,
              block_number, created_at_ms, execution_mode)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                id,
                position_id,
                side,
                reason,
                qty.to_string(),
                weth_wei.to_string(),
                gas_wei.to_string(),
                tx_hash,
                block_number.map(|b| b as i64),
                now_ms() as i64,
                execution_mode.as_str(),
            ],
        )?;
        Ok(())
    }

    /// Fills for one position, oldest first.
    pub fn sniper_fills(&self, position_id: &str) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, side, reason, qty, weth_wei, gas_wei, tx_hash, block_number,
                    created_at_ms, execution_mode
             FROM sniper_fills WHERE position_id = ?1 ORDER BY created_at_ms ASC",
        )?;
        let rows = stmt.query_map(params![position_id], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "side": row.get::<_, String>(1)?,
                "reason": row.get::<_, String>(2)?,
                "qty": row.get::<_, String>(3)?,
                "wethWei": row.get::<_, String>(4)?,
                "gasWei": row.get::<_, String>(5)?,
                "txHash": row.get::<_, Option<String>>(6)?,
                "blockNumber": row.get::<_, Option<i64>>(7)?,
                "createdAtMs": row.get::<_, i64>(8)?,
                "executionMode": row.get::<_, String>(9)?,
            }))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// The persisted paper bankroll. `None` means the ledger has never been
    /// written — a fresh checkout starts at exactly 1 ETH in memory.
    pub fn load_simulation_state(&self) -> Result<Option<(U256, u64, u64)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT balance_wei, reset_at_ms, updated_at_ms FROM sniper_simulation_state WHERE id = 1",
        )?;
        let mut rows = stmt.query([])?;
        match rows.next()? {
            Some(row) => Ok(Some((
                parse_u256_decimal(&row.get::<_, String>(0)?),
                row.get::<_, i64>(1)? as u64,
                row.get::<_, i64>(2)? as u64,
            ))),
            None => Ok(None),
        }
    }

    /// Persist the paper bankroll after any reserve/credit/reset. A reset is
    /// recorded with its timestamp so the audit trail shows when the bankroll
    /// was returned to 1 ETH — the history itself is never deleted.
    pub fn save_simulation_state(&self, balance_wei: U256, reset_at_ms: u64) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO sniper_simulation_state (id, balance_wei, reset_at_ms, updated_at_ms)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
                balance_wei   = excluded.balance_wei,
                reset_at_ms   = excluded.reset_at_ms,
                updated_at_ms = excluded.updated_at_ms",
            params![balance_wei.to_string(), reset_at_ms as i64, now_ms() as i64,],
        )?;
        Ok(())
    }

    /// The optimistic amounts recorded for a position's most recent sell
    /// fill — what the exit reconciliation needs to undo before booking the
    /// receipt's exact values.
    pub fn last_sell_fill_amounts(&self, position_id: &str) -> Result<Option<(U256, U256)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT qty, weth_wei FROM sniper_fills
              WHERE position_id = ?1 AND side = 'sell'
              ORDER BY created_at_ms DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![position_id])?;
        match rows.next()? {
            Some(row) => Ok(Some((
                parse_u256_decimal(&row.get::<_, String>(0)?),
                parse_u256_decimal(&row.get::<_, String>(1)?),
            ))),
            None => Ok(None),
        }
    }

    /// Correct the most recent sell fill of a position once the exit receipt
    /// is known: receipt-based exact accounting replaces the optimistic
    /// fill recorded at submission.
    pub fn correct_last_sell_fill(
        &self,
        position_id: &str,
        qty: U256,
        weth_wei: U256,
        gas_wei: U256,
        block_number: u64,
    ) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE sniper_fills
                SET qty = ?2, weth_wei = ?3, gas_wei = ?4, block_number = ?5
              WHERE id = (
                    SELECT id FROM sniper_fills
                     WHERE position_id = ?1 AND side = 'sell'
                     ORDER BY created_at_ms DESC LIMIT 1
              )",
            params![
                position_id,
                qty.to_string(),
                weth_wei.to_string(),
                gas_wei.to_string(),
                block_number as i64,
            ],
        )?;
        Ok(())
    }

    /// Remove the optimistic sell fill of an exit that reverted: a failed
    /// transaction books nothing.
    pub fn delete_last_sell_fill(&self, position_id: &str) -> Result<()> {
        self.conn.lock().execute(
            "DELETE FROM sniper_fills
              WHERE id = (
                    SELECT id FROM sniper_fills
                     WHERE position_id = ?1 AND side = 'sell'
                     ORDER BY created_at_ms DESC LIMIT 1
              )",
            params![position_id],
        )?;
        Ok(())
    }

    /// Remember a honeypot verdict so the same token is not re-probed.
    pub fn record_sniper_verdict(
        &self,
        token: &str,
        chain_id: u64,
        verdict: &str,
        round_trip_bps: Option<u32>,
        detail: &str,
    ) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO sniper_token_verdicts
             (token, chain_id, verdict, round_trip_bps, probed_at_ms, detail)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(token) DO UPDATE SET
                verdict        = excluded.verdict,
                round_trip_bps = excluded.round_trip_bps,
                probed_at_ms   = excluded.probed_at_ms,
                detail         = excluded.detail",
            params![
                token,
                chain_id as i64,
                verdict,
                round_trip_bps.map(|v| v as i64),
                now_ms() as i64,
                detail,
            ],
        )?;
        Ok(())
    }

    /// Tokens the probe has rejected outright — the persistent blacklist.
    pub fn sniper_honeypot_tokens(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT token FROM sniper_token_verdicts WHERE verdict = 'honeypot'")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Counts by verdict, for the sniper's evidence track.
    pub fn sniper_verdict_counts(&self) -> Result<std::collections::HashMap<String, u64>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT verdict, COUNT(*) FROM sniper_token_verdicts GROUP BY verdict")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }
}

fn parse_address(raw: &str) -> alloy_primitives::Address {
    raw.parse().unwrap_or(alloy_primitives::Address::ZERO)
}

fn parse_b256(raw: &str) -> Option<alloy_primitives::B256> {
    raw.parse().ok()
}

fn relative_error_bps(predicted: i128, observed: i128) -> u64 {
    let difference = U256::from(predicted.abs_diff(observed));
    let denominator = U256::from(observed.unsigned_abs().max(1));
    let ratio = difference * U256::from(10_000u64) / denominator;
    if ratio > U256::from(u64::MAX) {
        u64::MAX
    } else {
        ratio.to::<u64>()
    }
}

fn parse_i128_decimal(value: &str) -> i128 {
    value.parse::<i128>().unwrap_or_else(|_| {
        if value.trim_start().starts_with('-') {
            i128::MIN
        } else {
            i128::MAX
        }
    })
}

fn parse_u256_decimal(value: &str) -> U256 {
    value.parse::<U256>().unwrap_or(U256::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{now_ms, SimBackend};
    use alloy_primitives::Address;

    fn sim(strategy: Strategy, net: i128) -> SimulationResult {
        SimulationResult {
            opportunity_id: uuid::Uuid::new_v4().to_string(),
            strategy,
            backend: SimBackend::AnvilFork,
            success: net > 0,
            gross_profit_wei: U256::from(1_000u64),
            gas_used: 100_000,
            gas_price_wei: U256::from(20u64),
            gas_cost_wei: U256::from(500u64),
            bribe_wei: U256::ZERO,
            net_profit_wei: net,
            victim_predicted_out_wei: None,
            revert_reason: None,
            target_block: 100,
            sim_latency_ms: 10,
            created_at_ms: now_ms(),
        }
    }

    #[test]
    fn relative_error_is_exact_and_overflow_safe() {
        assert_eq!(relative_error_bps(100, 100), 0);
        assert_eq!(relative_error_bps(80, 100), 2_000);
        assert_eq!(relative_error_bps(120, 100), 2_000);
        assert_eq!(relative_error_bps(i128::MIN, i128::MAX), 20_000);
    }

    #[test]
    fn sequencer_actual_match_is_not_reused_as_independent_evidence() {
        let s = Store::open_in_memory().unwrap();
        let now = now_ms();
        let id = "atomic-one";
        {
            let conn = s.conn.lock();
            conn.execute(
                "INSERT INTO opportunities
                 (id,strategy,target_block,profit_token,expected_wei,notional_wei,victims,notes,created_at_ms)
                 VALUES (?1,'atomic_arb',100,'0x0','100','1000','[]','test',?2)",
                params![id, now as i64],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO simulations
                 (opportunity_id,strategy,backend,success,gross_wei,gas_used,gas_cost_wei,bribe_wei,net_wei,revert_reason,target_block,latency_ms,created_at_ms,reorged)
                 VALUES (?1,'atomic_arb','anvil_fork',1,'150',21000,'50','0','100',NULL,100,1,?2,0)",
                params![id, now as i64],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO actual_mev_matches
                 (opportunity_id,block_number,victim_hash,mev_tx_hashes,actor,gross_weth_wei,gas_cost_wei,net_weth_wei,confidence,confidence_score_bps,completeness,evidence,canonical,created_at_ms)
                 VALUES (?1,100,'','[]',NULL,'150','50','100','high',9000,'{}','{}',1,?2)",
                params![id, now as i64],
            )
            .unwrap();
        }
        let evidence = s
            .qualification_evidence(
                now.saturating_sub(1),
                Strategy::AtomicArb,
                8_000,
                crate::config::QualificationBackend::Sequencer,
            )
            .unwrap();
        assert_eq!(evidence.fork_samples, 1);
        assert!(
            evidence.relay_errors_bps.is_empty(),
            "the actual-match row is not an independent sequencer comparison"
        );
        assert_eq!(evidence.actual_errors_bps, vec![0]);
    }

    #[test]
    fn records_and_aggregates_pnl() {
        let s = Store::open_in_memory().unwrap();
        s.record_simulation(&sim(Strategy::Sandwich, 500)).unwrap();
        s.record_simulation(&sim(Strategy::Sandwich, -200)).unwrap();
        s.record_simulation(&sim(Strategy::AtomicArb, 1_000))
            .unwrap();

        let pnl = s.pnl().unwrap();
        assert_eq!(pnl.len(), 2);
        let sandwich = pnl.iter().find(|p| p.strategy == "sandwich").unwrap();
        assert_eq!(sandwich.simulations, 2);
        assert_eq!(sandwich.wins, 1);
        assert_eq!(sandwich.losses, 1);
        assert_eq!(sandwich.net_profit_wei, "300");
        assert_eq!(s.cumulative_net().unwrap(), 1_300);
    }

    #[test]
    fn opportunities_round_trip() {
        let s = Store::open_in_memory().unwrap();
        let o = Opportunity {
            id: "abc".into(),
            strategy: Strategy::Jit,
            victim_hashes: vec![],
            front_calls: vec![],
            back_calls: vec![],
            flash_tokens: vec![],
            flash_amounts: vec![],
            profit_token: Address::ZERO,
            expected_profit_wei: U256::from(7u8),
            notional_wei: U256::from(9u8),
            target_block: 5,
            created_at_ms: now_ms(),
            notes: "hello".into(),
        };
        s.record_opportunity(&o).unwrap();
        let got = s.recent_opportunities(10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["id"], "abc");
        assert_eq!(got[0]["notes"], "hello");
    }

    #[test]
    fn relay_bids_are_deduplicated_per_slot() {
        let s = Store::open_in_memory().unwrap();
        s.record_relay_bid("r", 1, "b", U256::from(5u8)).unwrap();
        s.record_relay_bid("r", 1, "b", U256::from(5u8)).unwrap();
        assert_eq!(s.recent_relay_bids(10).unwrap().len(), 1);
    }

    #[test]
    fn relay_blocks_and_txs_round_trip() {
        use crate::types::{PendingTx, RelayBlock, TxSource};
        use alloy_primitives::B256;

        let s = Store::open_in_memory().unwrap();
        let block = RelayBlock {
            relay: "https://bloxroute.max-profit.blxrbdn.com".into(),
            slot: 9_812_400,
            block_number: 21_000_000,
            block_hash: B256::from([7u8; 32]),
            builder: "0xbeef".into(),
            value_wei: U256::from(123u64),
            gas_used: 15_000_000,
            num_tx: 2,
        };
        let tx = PendingTx {
            hash: B256::from([9u8; 32]),
            from: Some(Address::with_last_byte(1)),
            to: Some(Address::with_last_byte(2)),
            value: U256::from(5u64),
            gas: 210_000,
            max_fee_per_gas: U256::from(20_000_000_000u64),
            max_priority_fee_per_gas: U256::from(1_000_000_000u64),
            nonce: 3,
            input: vec![0xde, 0xad, 0xbe, 0xef],
            raw: None,
            source: TxSource::RelayDelivered,
            mined_at: None,
            preconfirmed: None,
            seen_at_ms: now_ms(),
        };

        // Blocks dedup per (relay, slot); txs dedup per (block, hash).
        s.record_relay_block(&block).unwrap();
        s.record_relay_block(&block).unwrap();
        s.record_relay_block_tx(&block, &tx, 0).unwrap();
        s.record_relay_block_tx(&block, &tx, 0).unwrap();

        let blocks = s.recent_relay_blocks(10).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["blockNumber"], serde_json::json!(21_000_000));
        assert_eq!(blocks[0]["numTx"], serde_json::json!(2));
        assert_eq!(blocks[0]["valueWei"], serde_json::json!("123"));

        let txs = s.relay_block_txs(Some(21_000_000), 100).unwrap();
        assert_eq!(txs.len(), 1);
        assert_eq!(txs[0]["selector"], serde_json::json!("0xdeadbeef"));
        assert_eq!(
            txs[0]["from"],
            serde_json::json!(format!("{:?}", Address::with_last_byte(1)))
        );

        // Unknown block number → empty result.
        assert!(s.relay_block_txs(Some(1), 100).unwrap().is_empty());
    }

    #[test]
    fn reorged_simulations_drop_out_of_pnl() {
        let s = Store::open_in_memory().unwrap();
        s.record_simulation(&sim(Strategy::Sandwich, 500)).unwrap();
        s.record_simulation(&sim(Strategy::Sandwich, 200)).unwrap();
        s.record_reorg(100, 100, "0xold", "0xnew").unwrap();
        assert_eq!(s.cumulative_net().unwrap(), 0);
        assert!(s.pnl().unwrap().is_empty());
        let reorgs = s.recent_reorgs(10).unwrap();
        assert_eq!(reorgs.len(), 1);
        assert_eq!(reorgs[0]["depth"], serde_json::json!(1));
    }

    #[test]
    fn winning_bid_picks_the_highest_relay() {
        use crate::types::RelayBlock;
        use alloy_primitives::B256;
        let s = Store::open_in_memory().unwrap();
        let mut a = RelayBlock {
            relay: "a".into(),
            slot: 1,
            block_number: 50,
            block_hash: B256::from([1u8; 32]),
            builder: "x".into(),
            value_wei: U256::from(10u64),
            gas_used: 1,
            num_tx: 0,
        };
        s.record_relay_block(&a).unwrap();
        a.relay = "b".into();
        a.slot = 2;
        a.value_wei = U256::from(99u64);
        s.record_relay_block(&a).unwrap();
        assert_eq!(
            s.winning_bid_for_block(50).unwrap(),
            Some(U256::from(99u64))
        );
        assert_eq!(s.winning_bid_for_block(1).unwrap(), None);
    }

    // --- batched writes --------------------------------------------------

    fn relay_block(block_number: u64, num_tx: u64) -> crate::types::RelayBlock {
        use alloy_primitives::B256;
        crate::types::RelayBlock {
            relay: "test-relay".into(),
            slot: block_number,
            block_number,
            block_hash: B256::from([3u8; 32]),
            builder: "0xb".into(),
            value_wei: U256::from(1u64),
            gas_used: 1,
            num_tx,
        }
    }

    fn pending(nth: u8) -> crate::types::PendingTx {
        use alloy_primitives::B256;
        crate::types::PendingTx {
            hash: B256::from([nth; 32]),
            from: Some(Address::with_last_byte(nth)),
            to: Some(Address::with_last_byte(2)),
            value: U256::from(1u64),
            gas: 21_000,
            max_fee_per_gas: U256::ZERO,
            max_priority_fee_per_gas: U256::ZERO,
            nonce: nth as u64,
            input: vec![0xaa, 0xbb, 0xcc, 0xdd],
            raw: None,
            source: crate::types::TxSource::RelayDelivered,
            mined_at: None,
            preconfirmed: None,
            seen_at_ms: now_ms(),
        }
    }

    #[test]
    fn a_delivered_block_and_its_txs_commit_in_one_transaction() {
        let s = Store::open_in_memory().unwrap();
        let b = relay_block(21_000_001, 3);
        let txs: Vec<_> = (1..=3u8).map(pending).collect();
        s.record_relay_block_with_txs(&b, &txs).unwrap();

        assert_eq!(s.recent_relay_blocks(10).unwrap().len(), 1);
        let stored = s.relay_block_txs(Some(21_000_001), 100).unwrap();
        assert_eq!(stored.len(), 3);
        // Index order is preserved: replay ordering depends on it.
        assert_eq!(stored[0]["selector"], serde_json::json!("0xaabbccdd"));
    }

    #[test]
    fn the_bulk_relay_insert_is_idempotent() {
        // Delivered blocks can be re-fetched; re-inserting must not duplicate.
        let s = Store::open_in_memory().unwrap();
        let b = relay_block(21_000_002, 2);
        let txs: Vec<_> = (1..=2u8).map(pending).collect();
        s.record_relay_block_with_txs(&b, &txs).unwrap();
        s.record_relay_block_with_txs(&b, &txs).unwrap();
        assert_eq!(s.recent_relay_blocks(10).unwrap().len(), 1);
        assert_eq!(s.relay_block_txs(Some(21_000_002), 100).unwrap().len(), 2);
    }

    #[test]
    fn a_block_with_no_transactions_still_records_the_block() {
        let s = Store::open_in_memory().unwrap();
        s.record_relay_block_with_txs(&relay_block(21_000_003, 0), &[])
            .unwrap();
        assert_eq!(s.recent_relay_blocks(10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_async_writer_persists_queued_rows() {
        let store = std::sync::Arc::new(Store::open_in_memory().unwrap());
        let writes = AsyncStore::spawn(store.clone(), 256);
        for net in [100i128, -50, 900] {
            writes.record_simulation(&sim(Strategy::Sandwich, net));
        }
        assert_eq!(writes.dropped(), 0);
        assert_eq!(writes.queued(), 3);

        // The writer is a background task: poll until the batch lands.
        let mut found = 0;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            found = store.recent_simulations(10, None).unwrap().len();
            if found == 3 {
                break;
            }
        }
        assert_eq!(found, 3, "queued simulations should reach the store");
        // And they aggregate exactly as synchronous writes would.
        let pnl = store.pnl().unwrap();
        let sandwich = pnl.iter().find(|p| p.strategy == "sandwich").unwrap();
        assert_eq!(sandwich.simulations, 3);
        assert_eq!(sandwich.net_profit_wei, "950");
    }

    #[test]
    fn a_fresh_database_has_an_untripped_kill_switch() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.load_risk_state().unwrap(), PersistedRiskState::default());
    }

    #[test]
    fn the_kill_switch_round_trips_and_keeps_its_first_timestamp() {
        let s = Store::open_in_memory().unwrap();
        s.persist_kill_switch(true, -250).unwrap();
        let first = s.load_risk_state().unwrap();
        assert!(first.tripped);
        assert_eq!(first.cumulative_net_wei, -250);
        assert!(first.tripped_at_ms.is_some());

        // A later persist of the same trip must not rewrite the original time.
        std::thread::sleep(std::time::Duration::from_millis(2));
        s.persist_kill_switch(true, -400).unwrap();
        let again = s.load_risk_state().unwrap();
        assert_eq!(again.tripped_at_ms, first.tripped_at_ms);
        assert_eq!(again.cumulative_net_wei, -400);

        s.persist_kill_switch(false, 0).unwrap();
        let cleared = s.load_risk_state().unwrap();
        assert!(!cleared.tripped);
        assert_eq!(cleared.cumulative_net_wei, 0);
        assert!(cleared.tripped_at_ms.is_none());
    }

    #[test]
    fn smoke_slots_are_durable_and_capped() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.smoke_used().unwrap(), 0);
        assert!(s.try_consume_smoke_slot(2).unwrap());
        assert!(s.try_consume_smoke_slot(2).unwrap());
        assert!(!s.try_consume_smoke_slot(2).unwrap(), "budget exhausted");
        assert_eq!(s.smoke_used().unwrap(), 2);
        assert!(!s.try_consume_smoke_slot(0).unwrap(), "max=0 is off");
        assert_eq!(s.smoke_gas_at_risk_wei().unwrap(), U256::ZERO);
        // A kill-switch persist must not reset either smoke counter.
        s.persist_kill_switch(true, -1).unwrap();
        assert_eq!(s.smoke_used().unwrap(), 2);
        assert_eq!(s.smoke_gas_at_risk_wei().unwrap(), U256::ZERO);
    }

    #[test]
    fn raw_smoke_reserves_count_and_worst_case_gas_atomically() {
        let s = Store::open_in_memory().unwrap();
        let cap = U256::from(1_000u64);
        assert!(s
            .try_consume_smoke_budget(2, Some(cap), U256::from(400u64))
            .unwrap());
        assert_eq!(s.smoke_used().unwrap(), 1);
        assert_eq!(s.smoke_gas_at_risk_wei().unwrap(), U256::from(400u64));
        assert!(
            !s.try_consume_smoke_budget(2, Some(cap), U256::from(700u64))
                .unwrap(),
            "a send that exceeds the wei cap is refused without spending a slot"
        );
        assert_eq!(s.smoke_used().unwrap(), 1);
        assert_eq!(s.smoke_gas_at_risk_wei().unwrap(), U256::from(400u64));
        assert!(s
            .try_consume_smoke_budget(2, Some(cap), U256::from(600u64))
            .unwrap());
        assert_eq!(s.smoke_used().unwrap(), 2);
        assert_eq!(s.smoke_gas_at_risk_wei().unwrap(), cap);
        assert!(
            !s.try_consume_smoke_budget(3, Some(U256::ZERO), U256::from(1u64))
                .unwrap(),
            "zero raw gas budget is fail-closed"
        );
    }

    #[tokio::test]
    async fn a_full_write_queue_drops_instead_of_blocking() {
        // The guarantee that matters: the hot path is never blocked by
        // persistence. With a tiny queue and no writer draining it, sends
        // must return immediately and be counted as dropped.
        let (tx, _rx) = tokio::sync::mpsc::channel::<WriteOp>(1);
        let incident_at_ms = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let s = AsyncStore {
            tx,
            incident_at_ms: incident_at_ms.clone(),
            dropped: std::sync::atomic::AtomicU64::new(0),
            queued: std::sync::atomic::AtomicU64::new(0),
        };
        for net in 0..50i128 {
            s.record_simulation(&sim(Strategy::Jit, net));
        }
        assert_eq!(s.queued(), 1, "only the buffered slot is accepted");
        assert_eq!(s.dropped(), 49, "the rest are shed, not blocked");
        assert_ne!(
            incident_at_ms.load(std::sync::atomic::Ordering::Acquire),
            0,
            "the writer must durably invalidate the qualification window"
        );
    }

    #[test]
    fn state_comparison_is_independent_and_deduped() {
        let s = Store::open_in_memory().unwrap();
        let now = now_ms();
        let id = "arb-state-1";
        {
            let conn = s.conn.lock();
            conn.execute(
                "INSERT INTO opportunities
                 (id,strategy,target_block,profit_token,expected_wei,notional_wei,victims,notes,created_at_ms)
                 VALUES (?1,'atomic_arb',100,'0x0','100','1000','','test',?2)",
                params![id, now as i64],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO simulations
                 (opportunity_id,strategy,backend,success,gross_wei,gas_used,gas_cost_wei,bribe_wei,net_wei,revert_reason,target_block,latency_ms,created_at_ms,reorged)
                 VALUES (?1,'atomic_arb','anvil_fork',1,'150',21000,'50','0','100',NULL,100,1,?2,0)",
                params![id, now as i64],
            )
            .unwrap();
        }
        assert!(s
            .record_state_comparison(
                "sample-a",
                id,
                "atomic_arb",
                "head:100",
                100,
                "0xabc",
                "univ2:0x1 -> univ3:0x2",
                "1000",
                "weth->usdc->weth",
                100,
                100,
            )
            .unwrap());
        assert!(
            !s.record_state_comparison(
                "sample-a-dup",
                id,
                "atomic_arb",
                "head:100",
                100,
                "0xabc",
                "univ2:0x1 -> univ3:0x2",
                "1000",
                "weth->usdc->weth",
                100,
                80,
            )
            .unwrap(),
            "same (opp, state, route, amount, direction) must not count twice"
        );
        assert!(s
            .record_state_comparison(
                "sample-b",
                id,
                "atomic_arb",
                "head:100",
                100,
                "0xabc",
                "univ3:0x2 -> univ2:0x1",
                "1000",
                "weth->usdc->weth",
                100,
                90,
            )
            .unwrap());

        let evidence = s
            .qualification_evidence(
                now.saturating_sub(1),
                Strategy::AtomicArb,
                8_000,
                crate::config::QualificationBackend::Sequencer,
            )
            .unwrap();
        assert_eq!(evidence.fork_samples, 1);
        assert_eq!(evidence.relay_errors_bps.len(), 2);
        assert!(evidence.actual_errors_bps.is_empty());
    }

    #[test]
    fn reorged_state_comparisons_leave_the_independent_population() {
        let s = Store::open_in_memory().unwrap();
        s.record_state_comparison(
            "s1",
            "opp",
            "atomic_arb",
            "head:50",
            50,
            "0xold",
            "univ2:0x1",
            "1",
            "weth->usdc",
            10,
            10,
        )
        .unwrap();
        s.record_reorg(50, 50, "0xold", "0xnew").unwrap();
        let evidence = s
            .qualification_evidence(
                0,
                Strategy::AtomicArb,
                8_000,
                crate::config::QualificationBackend::Sequencer,
            )
            .unwrap();
        assert!(evidence.relay_errors_bps.is_empty());
    }

    #[test]
    fn wrong_direction_or_amount_is_a_different_sample() {
        let s = Store::open_in_memory().unwrap();
        assert!(s
            .record_state_comparison(
                "a",
                "o",
                "atomic_arb",
                "st",
                1,
                "0x",
                "r",
                "100",
                "weth->usdc",
                1,
                1
            )
            .unwrap());
        assert!(s
            .record_state_comparison(
                "b",
                "o",
                "atomic_arb",
                "st",
                1,
                "0x",
                "r",
                "200",
                "weth->usdc",
                1,
                1
            )
            .unwrap());
        assert!(s
            .record_state_comparison(
                "c",
                "o",
                "atomic_arb",
                "st",
                1,
                "0x",
                "r",
                "100",
                "usdc->weth",
                1,
                1
            )
            .unwrap());
        let evidence = s
            .qualification_evidence(
                0,
                Strategy::AtomicArb,
                8_000,
                crate::config::QualificationBackend::Sequencer,
            )
            .unwrap();
        assert_eq!(evidence.relay_errors_bps.len(), 3);
    }

    // --- directional sniper lane -------------------------------------------

    fn sniper_pos(id: &str, state: crate::sniper::PositionState) -> crate::sniper::Position {
        use alloy_primitives::Address;
        crate::sniper::Position {
            id: id.into(),
            chain_id: 1,
            token: Address::with_last_byte(0xAB),
            pair: Address::with_last_byte(0xCD),
            venue: "univ2".into(),
            state,
            trigger_tx: None,
            entry_tx: None,

            exit_tx: None,
            entry_cost_wei: U256::from(1_000_000_000_000_000_000u128),
            entry_qty: U256::from(4_242u64),
            remaining_qty: U256::from(4_242u64),
            realized_wei: U256::ZERO,
            gas_spent_wei: U256::from(777u64),
            peak_value_wei: U256::from(1_000_000_000_000_000_000u128),
            opened_block: 21_000_000,
            opened_at_ms: 1_700_000_000_000,
            closed_at_ms: None,
            exit_reason: None,
            entry_verdict: "clean".into(),
            notes: "backrun of addLiquidityETH".into(),
            execution_mode: crate::sniper::ExecutionMode::Live,
            settlement: crate::sniper::Settlement::OnChain,
            tx_status: crate::sniper::TxStatus::Mined,
        }
    }

    #[test]
    fn sniper_position_round_trips_through_sqlite() {
        let s = Store::open_in_memory().unwrap();
        let p = sniper_pos("p1", crate::sniper::PositionState::Open);
        s.upsert_sniper_position(&p).unwrap();

        let back = s.live_sniper_positions().unwrap();
        assert_eq!(back.len(), 1);
        let b = &back[0];
        assert_eq!(b.id, p.id);
        assert_eq!(b.token, p.token, "address must survive the round trip");
        assert_eq!(b.pair, p.pair);
        assert_eq!(b.entry_cost_wei, p.entry_cost_wei, "wei must not be lossy");
        assert_eq!(b.entry_qty, p.entry_qty);
        assert_eq!(b.gas_spent_wei, p.gas_spent_wei);
        assert_eq!(b.opened_block, p.opened_block);
        assert_eq!(b.state, crate::sniper::PositionState::Open);
        assert_eq!(b.entry_verdict, "clean");
        assert_eq!(b.notes, p.notes);
    }

    #[test]
    fn sniper_upsert_is_idempotent_and_updates_in_place() {
        let s = Store::open_in_memory().unwrap();
        let mut p = sniper_pos("p1", crate::sniper::PositionState::Open);
        s.upsert_sniper_position(&p).unwrap();

        p.apply_fill(U256::from(2_000u64), U256::from(500u64), U256::ZERO, 42);
        s.upsert_sniper_position(&p).unwrap();

        let back = s.live_sniper_positions().unwrap();
        assert_eq!(back.len(), 1, "upsert must not duplicate the row");
        assert_eq!(back[0].state, crate::sniper::PositionState::Scaling);
        assert_eq!(back[0].remaining_qty, U256::from(2_242u64));
        assert_eq!(back[0].realized_wei, U256::from(500u64));
    }

    #[test]
    fn only_live_positions_are_hydrated() {
        let s = Store::open_in_memory().unwrap();
        for (id, state) in [
            ("a", crate::sniper::PositionState::Open),
            ("b", crate::sniper::PositionState::Pending),
            ("c", crate::sniper::PositionState::Scaling),
            ("d", crate::sniper::PositionState::Closed),
            ("e", crate::sniper::PositionState::Abandoned),
        ] {
            s.upsert_sniper_position(&sniper_pos(id, state)).unwrap();
        }
        let live = s.live_sniper_positions().unwrap();
        assert_eq!(live.len(), 3);
        assert!(live.iter().all(|p| p.state.is_live()));
        assert_eq!(s.recent_sniper_positions(100).unwrap().len(), 5);
    }

    #[test]
    fn a_closed_position_round_trips_its_exit_reason() {
        let s = Store::open_in_memory().unwrap();
        let mut p = sniper_pos("p1", crate::sniper::PositionState::Closed);
        p.exit_reason = Some(crate::sniper::ExitReason::TrailingStop);
        p.closed_at_ms = Some(1_700_000_999_000);
        s.upsert_sniper_position(&p).unwrap();
        let back = &s.recent_sniper_positions(10).unwrap()[0];
        assert_eq!(
            back.exit_reason,
            Some(crate::sniper::ExitReason::TrailingStop)
        );
        assert_eq!(back.closed_at_ms, Some(1_700_000_999_000));
    }

    #[test]
    fn sniper_fills_append_and_read_back_in_order() {
        let s = Store::open_in_memory().unwrap();
        s.record_sniper_fill(
            "f1",
            "p1",
            "buy",
            "entry",
            U256::from(1_000u64),
            U256::from(50u64),
            U256::from(1u64),
            None,
            Some(100),
            crate::sniper::ExecutionMode::Live,
        )
        .unwrap();
        s.record_sniper_fill(
            "f2",
            "p1",
            "sell",
            "take_profit_pct",
            U256::from(500u64),
            U256::from(90u64),
            U256::from(1u64),
            None,
            Some(120),
            crate::sniper::ExecutionMode::Live,
        )
        .unwrap();
        let fills = s.sniper_fills("p1").unwrap();
        assert_eq!(fills.len(), 2);
        assert_eq!(fills[0]["side"], "buy");
        assert_eq!(fills[1]["reason"], "take_profit_pct");
        assert_eq!(fills[1]["qty"], "500");
    }

    #[test]
    fn a_replayed_fill_is_ignored() {
        let s = Store::open_in_memory().unwrap();
        for _ in 0..3 {
            s.record_sniper_fill(
                "f1",
                "p1",
                "buy",
                "entry",
                U256::from(1u64),
                U256::from(1u64),
                U256::ZERO,
                None,
                None,
                crate::sniper::ExecutionMode::Live,
            )
            .unwrap();
        }
        assert_eq!(s.sniper_fills("p1").unwrap().len(), 1);
    }

    #[test]
    fn sniper_provenance_round_trips_and_defaults_are_live_shaped() {
        let s = Store::open_in_memory().unwrap();
        let mut p = sniper_pos("sim1", crate::sniper::PositionState::Open);
        p.execution_mode = crate::sniper::ExecutionMode::Simulation;
        p.settlement = crate::sniper::Settlement::Paper;
        p.tx_status = crate::sniper::TxStatus::Intent;
        s.upsert_sniper_position(&p).unwrap();
        s.upsert_sniper_position(&sniper_pos("live1", crate::sniper::PositionState::Open))
            .unwrap();

        let all: std::collections::HashMap<_, _> = s
            .recent_sniper_positions(10)
            .unwrap()
            .into_iter()
            .map(|p| (p.id.clone(), p))
            .collect();
        let sim = &all["sim1"];
        assert_eq!(sim.execution_mode, crate::sniper::ExecutionMode::Simulation);
        assert_eq!(sim.settlement, crate::sniper::Settlement::Paper);
        assert_eq!(sim.tx_status, crate::sniper::TxStatus::Intent);
        let live = &all["live1"];
        assert_eq!(live.execution_mode, crate::sniper::ExecutionMode::Live);
        assert_eq!(live.settlement, crate::sniper::Settlement::OnChain);
    }

    #[test]
    fn legacy_paper_rows_are_backfilled_from_their_notes() {
        // A database written before the two-ledger model stored paper fills
        // with reason='simulation' and positions noted "SIMULATION ...". The
        // migration must relabel them rather than claim them as live.
        let s = Store::open_in_memory().unwrap();
        s.conn
            .lock()
            .execute(
                "UPDATE sniper_positions SET notes = 'SIMULATION paper entry' WHERE id = 'legacy'",
                [],
            )
            .ok();
        // Insert a legacy-shaped row directly (defaults: live/on_chain).
        let mut legacy = sniper_pos("legacy", crate::sniper::PositionState::Open);
        legacy.notes = "SIMULATION paper entry".into();
        s.upsert_sniper_position(&legacy).unwrap();
        // Simulate the migration on a pre-existing default row: reset it to
        // the SQL defaults, then run the backfill statement again.
        s.conn
            .lock()
            .execute(
                "UPDATE sniper_positions SET execution_mode = 'live', settlement = 'on_chain' WHERE id = 'legacy'",
                [],
            )
            .unwrap();
        s.conn
            .lock()
            .execute_batch(
                "UPDATE sniper_positions
                    SET execution_mode = 'simulation', settlement = 'paper'
                  WHERE execution_mode = 'live' AND notes LIKE 'SIMULATION%';",
            )
            .unwrap();
        let back = &s.recent_sniper_positions(10).unwrap()[0];
        assert_eq!(
            back.execution_mode,
            crate::sniper::ExecutionMode::Simulation
        );
        assert_eq!(back.settlement, crate::sniper::Settlement::Paper);
    }

    #[test]
    fn the_simulation_bankroll_persists_and_resets_explicitly() {
        let s = Store::open_in_memory().unwrap();
        assert!(
            s.load_simulation_state().unwrap().is_none(),
            "a fresh ledger has no persisted balance"
        );
        let one_eth = U256::from(1_000_000_000_000_000_000u128);
        s.save_simulation_state(one_eth, 0).unwrap();
        let half = U256::from(500_000_000_000_000_000u128);
        s.save_simulation_state(half, 0).unwrap();
        let (balance, reset_at, _) = s.load_simulation_state().unwrap().unwrap();
        assert_eq!(balance, half);
        assert_eq!(reset_at, 0);
        // An explicit reset rewrites the balance and stamps it — history rows
        // elsewhere are untouched by design.
        s.save_simulation_state(one_eth, 1_234).unwrap();
        let (balance, reset_at, _) = s.load_simulation_state().unwrap().unwrap();
        assert_eq!(balance, one_eth);
        assert_eq!(reset_at, 1_234);
    }

    #[test]
    fn exit_tx_round_trips_for_receipt_reconciliation() {
        let s = Store::open_in_memory().unwrap();
        let mut p = sniper_pos("x1", crate::sniper::PositionState::Scaling);
        p.tx_status = crate::sniper::TxStatus::Submitted;
        p.exit_tx = Some(alloy_primitives::B256::repeat_byte(0x77));
        s.upsert_sniper_position(&p).unwrap();
        let back = &s.recent_sniper_positions(10).unwrap()[0];
        assert_eq!(
            back.exit_tx,
            Some(alloy_primitives::B256::repeat_byte(0x77))
        );
        assert_eq!(back.tx_status, crate::sniper::TxStatus::Submitted);
    }

    #[test]
    fn optimistic_sell_fills_are_corrected_or_deleted_from_receipts() {
        let s = Store::open_in_memory().unwrap();
        s.record_sniper_fill(
            "sb",
            "p9",
            "buy",
            "entry",
            U256::from(1_000u64),
            U256::from(100u64),
            U256::ZERO,
            None,
            Some(1),
            crate::sniper::ExecutionMode::Live,
        )
        .unwrap();
        s.record_sniper_fill(
            "ss",
            "p9",
            "sell",
            "take_profit_pct",
            U256::from(1_000u64),
            U256::from(150u64),
            U256::ZERO,
            Some("0xabc".into()),
            Some(2),
            crate::sniper::ExecutionMode::Live,
        )
        .unwrap();

        // Reconciliation replaces the optimistic amounts with the receipt's.
        let (qty, weth) = s.last_sell_fill_amounts("p9").unwrap().unwrap();
        assert_eq!(qty, U256::from(1_000u64));
        assert_eq!(weth, U256::from(150u64));
        s.correct_last_sell_fill(
            "p9",
            U256::from(990u64),
            U256::from(148u64),
            U256::from(3u64),
            9,
        )
        .unwrap();
        let (qty, weth) = s.last_sell_fill_amounts("p9").unwrap().unwrap();
        assert_eq!(qty, U256::from(990u64));
        assert_eq!(weth, U256::from(148u64));
        // The buy fill is untouched.
        assert_eq!(s.sniper_fills("p9").unwrap().len(), 2);

        // A reverted exit deletes the optimistic fill entirely.
        s.delete_last_sell_fill("p9").unwrap();
        let fills = s.sniper_fills("p9").unwrap();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0]["side"], "buy");
        assert!(s.last_sell_fill_amounts("p9").unwrap().is_none());
    }

    #[test]
    fn fill_provenance_is_recorded_per_fill() {
        let s = Store::open_in_memory().unwrap();
        s.record_sniper_fill(
            "fs",
            "p1",
            "buy",
            "simulation",
            U256::from(1u64),
            U256::from(1u64),
            U256::ZERO,
            None,
            None,
            crate::sniper::ExecutionMode::Simulation,
        )
        .unwrap();
        let fills = s.sniper_fills("p1").unwrap();
        assert_eq!(fills[0]["executionMode"], "simulation");
    }

    #[test]
    fn honeypot_verdicts_persist_as_a_blacklist() {
        let s = Store::open_in_memory().unwrap();
        s.record_sniper_verdict("0xdead", 1, "honeypot", None, "sell reverted")
            .unwrap();
        s.record_sniper_verdict("0xbeef", 1, "clean", Some(9_940), "")
            .unwrap();
        let blacklist = s.sniper_honeypot_tokens().unwrap();
        assert_eq!(blacklist, vec!["0xdead".to_string()]);

        let counts = s.sniper_verdict_counts().unwrap();
        assert_eq!(counts.get("honeypot"), Some(&1));
        assert_eq!(counts.get("clean"), Some(&1));
    }

    #[test]
    fn a_reprobed_token_updates_rather_than_duplicating() {
        let s = Store::open_in_memory().unwrap();
        s.record_sniper_verdict("0xdead", 1, "clean", Some(9_900), "")
            .unwrap();
        s.record_sniper_verdict("0xdead", 1, "honeypot", None, "went dark")
            .unwrap();
        let counts = s.sniper_verdict_counts().unwrap();
        assert_eq!(counts.get("clean"), None);
        assert_eq!(counts.get("honeypot"), Some(&1));
    }
}
