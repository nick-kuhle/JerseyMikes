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
    pub net_wei: i64,
    pub bribe_wei: String,
    pub block_number: u64,
    /// Comma-separated `0x…` hashes; empty means the opportunity had no victim
    /// (arb / liquidation / sniper).
    pub victims: String,
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
    pub net_profit_wei: i64,
    pub best_net_wei: i64,
    pub worst_net_wei: i64,
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
                net_wei        INTEGER NOT NULL,
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
                sim_net_wei     INTEGER NOT NULL,
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
            "#,
        )?;
        // Additive columns for databases created before Phase 1. SQLite has no
        // IF NOT EXISTS for columns; a duplicate-column error is the success
        // case on a second boot.
        self.add_column("blocks", "parent_hash", "TEXT NOT NULL DEFAULT ''");
        self.add_column("blocks", "canonical", "INTEGER NOT NULL DEFAULT 1");
        self.add_column("simulations", "reorged", "INTEGER NOT NULL DEFAULT 0");
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
        let this = std::sync::Arc::new(Self {
            tx,
            dropped: std::sync::atomic::AtomicU64::new(0),
            queued: std::sync::atomic::AtomicU64::new(0),
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
                clamp_i64(s.net_profit_wei),
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
            "SELECT s.opportunity_id, s.strategy, s.success, s.net_wei, s.bribe_wei,
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
                net_wei: row.get(3)?,
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
        sim_net_wei: i64,
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
                sim_net_wei,
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
            "SELECT block_number, opportunity_id, strategy, sim_net_wei, our_bribe_wei, winning_bid_wei,
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
                "simNetWei": row.get::<_, i64>(3)?,
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

    /// PnL per strategy, computed over the primary (fork) simulations only so
    /// the relay cross-check never double counts.
    pub fn pnl(&self) -> Result<Vec<PnlSummary>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT strategy,
                    COUNT(*),
                    SUM(CASE WHEN net_wei > 0 THEN 1 ELSE 0 END),
                    SUM(CASE WHEN net_wei <= 0 THEN 1 ELSE 0 END),
                    COALESCE(SUM(CAST(gross_wei AS REAL)), 0),
                    COALESCE(SUM(CAST(gas_cost_wei AS REAL)), 0),
                    COALESCE(SUM(net_wei), 0),
                    COALESCE(MAX(net_wei), 0),
                    COALESCE(MIN(net_wei), 0),
                    COALESCE(AVG(latency_ms), 0)
             FROM simulations
             WHERE backend = 'anvil_fork' AND COALESCE(reorged, 0) = 0
             GROUP BY strategy",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(PnlSummary {
                strategy: row.get(0)?,
                simulations: row.get(1)?,
                wins: row.get(2)?,
                losses: row.get(3)?,
                gross_profit_wei: format!("{:.0}", row.get::<_, f64>(4)?),
                gas_spent_wei: format!("{:.0}", row.get::<_, f64>(5)?),
                net_profit_wei: row.get(6)?,
                best_net_wei: row.get(7)?,
                worst_net_wei: row.get(8)?,
                avg_latency_ms: row.get(9)?,
            })
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Cumulative net PnL in wei across every fork simulation.
    pub fn cumulative_net(&self) -> Result<i64> {
        let conn = self.conn.lock();
        let v: i64 = conn.query_row(
            "SELECT COALESCE(SUM(net_wei), 0) FROM simulations WHERE backend = 'anvil_fork' AND COALESCE(reorged, 0) = 0",
            [],
            |r| r.get(0),
        )?;
        Ok(v)
    }

    pub fn recent_simulations(
        &self,
        limit: i64,
        strategy: Option<Strategy>,
    ) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock();
        let sql = "SELECT s.opportunity_id, s.strategy, s.backend, s.success, s.gross_wei, s.gas_used,
                          s.gas_cost_wei, s.bribe_wei, s.net_wei, s.revert_reason, s.target_block,
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
                "netWei": row.get::<_, i64>(8)?,
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

    /// Net PnL bucketed by block, for the equity curve.
    pub fn pnl_series(&self, limit: i64) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT target_block, SUM(net_wei), COUNT(*)
             FROM simulations WHERE backend = 'anvil_fork' AND COALESCE(reorged, 0) = 0
             GROUP BY target_block ORDER BY target_block DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok(serde_json::json!({
                "block": row.get::<_, i64>(0)?,
                "netWei": row.get::<_, i64>(1)?,
                "count": row.get::<_, i64>(2)?,
            }))
        })?;
        let mut v: Vec<serde_json::Value> = rows.filter_map(|r| r.ok()).collect();
        v.reverse();
        Ok(v)
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

fn clamp_i64(v: i128) -> i64 {
    if v > i64::MAX as i128 {
        i64::MAX
    } else if v < i64::MIN as i128 {
        i64::MIN
    } else {
        v as i64
    }
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
        assert_eq!(sandwich.net_profit_wei, 300);
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
        assert_eq!(sandwich.net_profit_wei, 950);
    }

    #[tokio::test]
    async fn a_full_write_queue_drops_instead_of_blocking() {
        // The guarantee that matters: the hot path is never blocked by
        // persistence. With a tiny queue and no writer draining it, sends
        // must return immediately and be counted as dropped.
        let (tx, _rx) = tokio::sync::mpsc::channel::<WriteOp>(1);
        let s = AsyncStore {
            tx,
            dropped: std::sync::atomic::AtomicU64::new(0),
            queued: std::sync::atomic::AtomicU64::new(0),
        };
        for net in 0..50i128 {
            s.record_simulation(&sim(Strategy::Jit, net));
        }
        assert_eq!(s.queued(), 1, "only the buffered slot is accepted");
        assert_eq!(s.dropped(), 49, "the rest are shed, not blocked");
    }
}
