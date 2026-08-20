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

use crate::types::{BundleRecord, Opportunity, SimulationResult, Strategy};

pub struct Store {
    conn: Mutex<Connection>,
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
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
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
                base_fee_wei  TEXT NOT NULL,
                gas_used      INTEGER NOT NULL,
                timestamp     INTEGER NOT NULL,
                seen_at_ms    INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS relay_bids (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                relay         TEXT NOT NULL,
                slot          INTEGER NOT NULL,
                builder       TEXT NOT NULL,
                value_wei     TEXT NOT NULL,
                seen_at_ms    INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_sim_strategy ON simulations(strategy);
            CREATE INDEX IF NOT EXISTS idx_sim_created ON simulations(created_at_ms);
            CREATE INDEX IF NOT EXISTS idx_opp_created ON opportunities(created_at_ms);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_relay_slot ON relay_bids(relay, slot);
            "#,
        )?;
        Ok(())
    }

    pub fn record_opportunity(&self, o: &Opportunity) -> Result<()> {
        self.conn.lock().execute(
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

    pub fn record_simulation(&self, s: &SimulationResult) -> Result<()> {
        self.conn.lock().execute(
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

    pub fn record_bundle(&self, b: &BundleRecord) -> Result<()> {
        self.conn.lock().execute(
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

    pub fn record_block(&self, head: &crate::types::BlockHead) -> Result<()> {
        self.conn.lock().execute(
            "INSERT OR REPLACE INTO blocks (number, hash, base_fee_wei, gas_used, timestamp, seen_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                head.number as i64,
                format!("{:?}", head.hash),
                head.base_fee_per_gas.to_string(),
                head.gas_used as i64,
                head.timestamp as i64,
                crate::types::now_ms() as i64,
            ],
        )?;
        Ok(())
    }

    pub fn record_relay_bid(&self, relay: &str, slot: u64, builder: &str, value: U256) -> Result<()> {
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
             WHERE backend = 'anvil_fork'
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
            "SELECT COALESCE(SUM(net_wei), 0) FROM simulations WHERE backend = 'anvil_fork'",
            [],
            |r| r.get(0),
        )?;
        Ok(v)
    }

    pub fn recent_simulations(&self, limit: i64, strategy: Option<Strategy>) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock();
        let sql = "SELECT s.opportunity_id, s.strategy, s.backend, s.success, s.gross_wei, s.gas_used,
                          s.gas_cost_wei, s.bribe_wei, s.net_wei, s.revert_reason, s.target_block,
                          s.latency_ms, s.created_at_ms, COALESCE(o.notes, '')
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
             FROM simulations WHERE backend = 'anvil_fork'
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
        s.record_simulation(&sim(Strategy::AtomicArb, 1_000)).unwrap();

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
}
