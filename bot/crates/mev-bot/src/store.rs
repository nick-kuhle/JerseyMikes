//! SQLite persistence.
//!
//! Writes are small and synchronous (SQLite in WAL mode handles thousands of
//! inserts a second), so the store is a plain mutex-guarded connection rather
//! than an async pool. Every table is append-only, which keeps the dashboard's
//! history queries trivial and makes post-hoc analysis of a run possible.

use std::path::Path;

use alloy_primitives::U256;
use anyhow::Result;
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
pub struct SubmittedBundle {
    pub bundle_id: String,
    pub opportunity_id: String,
    pub target_block: u64,
    pub tx_hashes: Vec<alloy_primitives::B256>,
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
            CREATE INDEX IF NOT EXISTS idx_opp_created ON opportunities(created_at_ms);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_relay_slot ON relay_bids(relay, slot);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_relay_block_slot ON relay_blocks(relay, slot);
            CREATE INDEX IF NOT EXISTS idx_relay_block_txs_block ON relay_block_txs(block_number);
            CREATE INDEX IF NOT EXISTS idx_recon_block ON reconciliations(block_number);
            CREATE INDEX IF NOT EXISTS idx_actual_mev_block ON actual_mev_matches(block_number);
            CREATE INDEX IF NOT EXISTS idx_submission_bundle ON relay_submissions(bundle_id);
            "#,
        )?;
        // Additive columns for databases created before Phase 1. SQLite has no
        // IF NOT EXISTS for columns; a duplicate-column error is the success
        // case on a second boot.
        self.add_column("blocks", "parent_hash", "TEXT NOT NULL DEFAULT ''");
        self.add_column("blocks", "canonical", "INTEGER NOT NULL DEFAULT 1");
        self.add_column("simulations", "reorged", "INTEGER NOT NULL DEFAULT 0");
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
    incident_tx: tokio::sync::mpsc::UnboundedSender<u64>,
    incident_recorded: std::sync::atomic::AtomicBool,
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
        let (incident_tx, mut incident_rx) = tokio::sync::mpsc::unbounded_channel::<u64>();
        let this = std::sync::Arc::new(Self {
            tx,
            incident_tx,
            incident_recorded: std::sync::atomic::AtomicBool::new(false),
            dropped: std::sync::atomic::AtomicU64::new(0),
            queued: std::sync::atomic::AtomicU64::new(0),
        });
        let incident_store = store.clone();
        tokio::task::spawn_blocking(move || {
            while let Some(at_ms) = incident_rx.blocking_recv() {
                if let Err(error) = incident_store.record_qualification_incident(
                    "persistence_drop",
                    "one or more bounded telemetry writes were dropped",
                    at_ms,
                ) {
                    tracing::error!(target: "store", %error, "could not persist qualification incident");
                }
            }
        });
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
                drop(conn);
                batch.clear();
            }
            tracing::info!(target: "store", "writer task stopped");
        });
        this
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
                if !self
                    .incident_recorded
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    let _ = self.incident_tx.send(now_ms());
                }
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
                if !self
                    .incident_recorded
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    let _ = self.incident_tx.send(now_ms());
                }
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
              net_wei, revert_reason, target_block, latency_ms, created_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
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
            "SELECT id, opportunity_id, target_block, payload FROM bundles
             WHERE submitted = 1 AND included IS NULL AND target_block <= ?1
             ORDER BY target_block ASC LIMIT 500",
        )?;
        let rows = stmt.query_map(params![block_number as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u64,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for (bundle_id, opportunity_id, target_block, payload) in rows.flatten() {
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
        observed_hashes: &[alloy_primitives::B256],
    ) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE bundles SET inclusion_state = ?2, observed_tx_hashes = ?3,
                    inclusion_checked_ms = ?4 WHERE id = ?1",
            params![
                bundle_id,
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
    pub fn qualification_evidence(
        &self,
        since_ms: u64,
        strategy: Strategy,
        minimum_confidence_bps: u64,
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
        let relay_rows = relay_stmt.query_map(params![strategy, since_ms as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let relay_errors_bps = relay_rows
            .flatten()
            .map(|(fork, relay)| {
                relative_error_bps(parse_i128_decimal(&fork), parse_i128_decimal(&relay))
            })
            .collect();

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

    #[tokio::test]
    async fn a_full_write_queue_drops_instead_of_blocking() {
        // The guarantee that matters: the hot path is never blocked by
        // persistence. With a tiny queue and no writer draining it, sends
        // must return immediately and be counted as dropped.
        let (tx, _rx) = tokio::sync::mpsc::channel::<WriteOp>(1);
        let (incident_tx, mut incident_rx) = tokio::sync::mpsc::unbounded_channel();
        let s = AsyncStore {
            tx,
            incident_tx,
            incident_recorded: std::sync::atomic::AtomicBool::new(false),
            dropped: std::sync::atomic::AtomicU64::new(0),
            queued: std::sync::atomic::AtomicU64::new(0),
        };
        for net in 0..50i128 {
            s.record_simulation(&sim(Strategy::Jit, net));
        }
        assert_eq!(s.queued(), 1, "only the buffered slot is accepted");
        assert_eq!(s.dropped(), 49, "the rest are shed, not blocked");
        assert!(
            incident_rx.try_recv().is_ok(),
            "the window is durably invalidated"
        );
        assert!(
            incident_rx.try_recv().is_err(),
            "one incident per process is enough"
        );
    }
}
