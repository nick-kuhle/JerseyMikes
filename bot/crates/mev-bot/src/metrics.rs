//! Prometheus text rendering of the engine's existing snapshots.
//!
//! `/api/metrics` serves `text/plain; version=0.0.4` so any scraper —
//! Prometheus, VictoriaMetrics, `curl` in a cron — can watch the bot without
//! another client library. The input is the same JSON the API already
//! produces (`stats.snapshot()`, latency, funnel, inventory, …); the
//! renderer walks it and emits one sample per scalar. Maps of counters
//! (`{strategy: {candidatesEmitted: …}}`) render with labels, which is what
//! makes per-strategy funnel lines plottable:
//!
//! ```text
//! mev_funnel_submittable{lane="live",strategy="sandwich"} 4
//! ```

use serde_json::Value;

/// Render a JSON tree as Prometheus samples with the given metric prefix.
/// Numbers and booleans become samples (bool → 0/1); strings, arrays and
/// nulls are skipped unless the string is numeric.
pub fn render(root: &Value, prefix: &str) -> String {
    let mut out = String::new();
    walk(root, prefix, &mut out);
    out
}

/// Render a funnel-shaped map (`{strategy: {counter: n}}`) as labelled
/// samples with a provenance lane: the two lanes must never be summed.
pub fn render_funnel(map: &Value, prefix: &str, lane: &str) -> String {
    let mut out = String::new();
    if let Some(map) = map.as_object() {
        for (strategy, counters) in map {
            let Some(counters) = counters.as_object() else {
                continue;
            };
            for (field, leaf) in counters {
                if let Some(sample) = scalar(leaf) {
                    out.push_str(&format!(
                        "{}_{}{{lane=\"{}\",strategy=\"{}\"}} {}\n",
                        prefix,
                        to_snake(field),
                        escape(lane),
                        escape(strategy),
                        sample
                    ));
                }
            }
        }
    }
    out
}

fn walk(node: &Value, path: &str, out: &mut String) {
    match node {
        Value::Number(n) => {
            out.push_str(&format!("{} {}\n", path, n));
        }
        Value::Bool(b) => {
            out.push_str(&format!("{} {}\n", path, if *b { 1 } else { 0 }));
        }
        Value::String(s) => {
            // Wei amounts are serialised as strings to keep JSON precision;
            // render them as samples when they are plain integers.
            if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || c == '-') {
                out.push_str(&format!("{} {}\n", path, s));
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().take(64).enumerate() {
                walk(item, &format!("{path}_{i}"), out);
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                walk(v, &format!("{}_{}", path, to_snake(k)), out);
            }
        }
        Value::Null => {}
    }
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(if *b { "1".into() } else { "0".into() }),
        Value::String(s) if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || c == '-') => {
            Some(s.clone())
        }
        _ => None,
    }
}

fn to_snake(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flattens_scalars_and_skips_non_numeric_strings() {
        let v = json!({
            "mode": "simulation",          // string: skipped
            "blocksSeen": 1200,
            "liveArmed": false,
            "head": {"number": 100, "hash": "0xdead"},   // hash skipped
        });
        let text = render(&v, "mev");
        assert!(text.contains("mev_blocks_seen 1200\n"), "{text}");
        assert!(text.contains("mev_live_armed 0\n"), "{text}");
        assert!(text.contains("mev_head_number 100\n"), "{text}");
        assert!(!text.contains("mode"));
        assert!(!text.contains("dead"));
    }

    #[test]
    fn wei_strings_render_as_samples() {
        let v = json!({"minNetProfitWei": "1000000000000000"});
        assert!(render(&v, "mev").contains("mev_min_net_profit_wei 1000000000000000\n"));
    }

    #[test]
    fn funnel_maps_render_with_lane_and_strategy_labels() {
        let v = json!({
            "sandwich": {"candidatesEmitted": 10, "submittable": 2},
            "sniper": {"candidatesEmitted": 5, "submittable": 0},
        });
        let text = render_funnel(&v, "mev_funnel", "live");
        assert!(
            text.contains("mev_funnel_submittable{lane=\"live\",strategy=\"sandwich\"} 2\n"),
            "{text}"
        );
        assert!(
            text.contains("mev_funnel_candidates_emitted{lane=\"live\",strategy=\"sniper\"} 5\n"),
            "{text}"
        );
    }

    #[test]
    fn nested_objects_keep_flattening() {
        let v = json!({"a": {"x": 1}, "b": {"y": {"z": 2}}});
        let text = render(&v, "mev");
        assert!(text.contains("mev_a_x 1\n"), "{text}");
        assert!(text.contains("mev_b_y_z 2\n"), "{text}");
    }

    #[test]
    fn funnel_label_values_are_escaped() {
        let v = json!({"we\"ird": {"n": 1}});
        assert!(render_funnel(&v, "mev_funnel", "live").contains("strategy=\"we\\\"ird\""));
    }
}
