//! The sniper's own execution mode: `Simulation | Live`.
//!
//! This is deliberately **not** the atomic engine's mode. The two lanes have
//! different contracts, different signers, different risk envelopes and
//! different worst cases, so their simulation/live switches are independent:
//!
//! | Atomic MEV | Sniper | Meaning |
//! | --- | --- | --- |
//! | Simulation | Simulation | Entire system paper-only |
//! | Live | Simulation | Atomic engine live; sniper paper-only |
//! | Simulation | Live | Sniper live; atomic engine paper-only |
//! | Live | Live | Both live, each behind its own gates |
//!
//! The mode is explicit state, read from `SNIPER_MODE` at boot and flipped at
//! runtime through `GET/POST /api/sniper/mode`. It is never derived from
//! `/api/mode` or `LIVE_EXECUTION` at request time.
//!
//! ## Boot ceiling
//!
//! `SNIPER_LIVE_ENABLED=true` means only that the process was *booted with
//! live sniper capability*. It never starts trading by itself: a fresh
//! checkout boots into `simulation`, and switching to `live` at runtime
//! additionally requires a configured and verified production vault, the
//! dedicated sniper key, and valid budgets — see
//! [`SniperLane::live_switch_blockers`](super::SniperLane::live_switch_blockers).

use serde::{Deserialize, Serialize};

/// Execution mode of the directional sniper lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SniperMode {
    /// Contract-backed trades against a local Anvil deployment of the real
    /// `SniperVault` bytecode, settling against the 1 ETH paper bankroll.
    /// Cannot spend real funds.
    Simulation,
    /// Signed submissions to the configured production vault on the selected
    /// chain, bounded by every live guard.
    Live,
}

impl SniperMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SniperMode::Simulation => "simulation",
            SniperMode::Live => "live",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "simulation" | "sim" | "paper" => Some(SniperMode::Simulation),
            "live" => Some(SniperMode::Live),
            _ => None,
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(self, SniperMode::Live)
    }
}

impl std::fmt::Display for SniperMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Boot-time capability for the sniper lane, read once from the environment.
///
/// Mirrors the atomic engine's two-key arming (`LIVE_EXECUTION` +
/// `I_UNDERSTAND_LIVE_RISK`): the runtime switch can only narrow what the
/// boot allowed, never widen it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SniperModeBoot {
    /// The mode the process starts in (`SNIPER_MODE`, default `simulation`).
    pub mode: SniperMode,
    /// `SNIPER_LIVE_ENABLED` — capability only, not trading.
    pub live_enabled: bool,
}

impl Default for SniperModeBoot {
    /// A fresh checkout is paper-only: simulation mode, no live capability.
    fn default() -> Self {
        Self {
            mode: SniperMode::Simulation,
            live_enabled: false,
        }
    }
}

impl SniperModeBoot {
    /// Parse and validate the boot envelope, failing closed.
    ///
    /// Errors are returned as human-readable blockers rather than panics so
    /// `Config::validate` can surface them alongside every other problem.
    pub fn from_env_parts(
        mode_raw: Option<&str>,
        live_enabled: bool,
        sniper_key_configured: bool,
    ) -> Result<Self, Vec<String>> {
        let mut errs = Vec::new();
        let mode = match mode_raw {
            None => SniperMode::Simulation,
            Some(raw) => match SniperMode::parse(raw) {
                Some(m) => m,
                None => {
                    errs.push(format!(
                        "SNIPER_MODE={raw:?} is not valid — use \"simulation\" or \"live\""
                    ));
                    SniperMode::Simulation
                }
            },
        };
        if mode == SniperMode::Live {
            if !live_enabled {
                errs.push(
                    "SNIPER_MODE=live requires SNIPER_LIVE_ENABLED=true at boot — the live \
                     ceiling is a deliberate restart-time decision"
                        .to_string(),
                );
            }
            if !sniper_key_configured {
                errs.push(
                    "SNIPER_MODE=live requires SNIPER_SEARCHER_PRIVATE_KEY — booting live \
                     without the dedicated sniper signer is refused"
                        .to_string(),
                );
            }
        }
        if errs.is_empty() {
            Ok(Self { mode, live_enabled })
        } else {
            Err(errs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_checkout_boots_paper_only() {
        let boot = SniperModeBoot::default();
        assert_eq!(boot.mode, SniperMode::Simulation);
        assert!(!boot.live_enabled);
    }

    #[test]
    fn mode_parses_and_round_trips() {
        assert_eq!(
            SniperMode::parse("simulation"),
            Some(SniperMode::Simulation)
        );
        assert_eq!(SniperMode::parse("LIVE"), Some(SniperMode::Live));
        assert_eq!(SniperMode::parse("paper"), Some(SniperMode::Simulation));
        assert_eq!(SniperMode::parse("shadow"), None);
        assert_eq!(SniperMode::Live.as_str(), "live");
        assert_eq!(SniperMode::Simulation.as_str(), "simulation");
    }

    #[test]
    fn mode_serialises_as_snake_case_strings() {
        assert_eq!(
            serde_json::to_value(SniperMode::Simulation).unwrap(),
            serde_json::json!("simulation")
        );
        assert_eq!(
            serde_json::to_value(SniperMode::Live).unwrap(),
            serde_json::json!("live")
        );
        let back: SniperMode = serde_json::from_str("\"live\"").unwrap();
        assert_eq!(back, SniperMode::Live);
    }

    #[test]
    fn live_boot_without_the_ceiling_fails_closed() {
        let errs = SniperModeBoot::from_env_parts(Some("live"), false, true).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("SNIPER_LIVE_ENABLED")));
    }

    #[test]
    fn live_boot_without_the_sniper_key_fails_closed() {
        let errs = SniperModeBoot::from_env_parts(Some("live"), true, false).unwrap_err();
        assert!(errs
            .iter()
            .any(|e| e.contains("SNIPER_SEARCHER_PRIVATE_KEY")));
    }

    #[test]
    fn live_boot_with_both_keys_succeeds() {
        let boot = SniperModeBoot::from_env_parts(Some("live"), true, true).unwrap();
        assert_eq!(boot.mode, SniperMode::Live);
        assert!(boot.live_enabled);
    }

    #[test]
    fn simulation_boot_never_requires_live_capability() {
        // The sniper can simulate even on a process with zero live arming —
        // that is the whole point of the independent mode.
        let boot = SniperModeBoot::from_env_parts(Some("simulation"), false, false).unwrap();
        assert_eq!(boot.mode, SniperMode::Simulation);
        // And an omitted SNIPER_MODE defaults to simulation.
        let boot = SniperModeBoot::from_env_parts(None, false, false).unwrap();
        assert_eq!(boot.mode, SniperMode::Simulation);
    }

    #[test]
    fn garbage_mode_values_are_reported_not_guessed() {
        let errs = SniperModeBoot::from_env_parts(Some("yolo"), false, false).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("SNIPER_MODE")));
    }

    #[test]
    fn live_enabled_alone_is_capability_not_trading() {
        // SNIPER_LIVE_ENABLED=true with the default mode must still boot in
        // simulation: the ceiling grants the *ability to switch*, nothing more.
        let boot = SniperModeBoot::from_env_parts(None, true, true).unwrap();
        assert_eq!(boot.mode, SniperMode::Simulation);
        assert!(boot.live_enabled);
    }
}
