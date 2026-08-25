//! Minimal, dependency-light JSON-RPC 2.0 client (HTTP + WebSocket subscriptions).
//!
//! We deliberately avoid a heavyweight provider stack: the bot only needs a
//! handful of methods, and owning the transport makes it trivial to talk to
//! non-standard endpoints (anvil, Flashbots relay, builder RPCs, sequencer
//! feeds) that ship their own namespaces.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::Address;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct RpcClient {
    http: reqwest::Client,
    url: String,
    id: std::sync::Arc<AtomicU64>,
    /// Extra headers (used for the Flashbots signature header).
    headers: Vec<(String, String)>,
    /// Data-plane health counters (work order 0.3), shared by every clone.
    stats: std::sync::Arc<RpcStats>,
}

/// Per-endpoint data-plane health counters (work order 0.3). All plain
/// atomics: the console reads them on every status tick and a slow panel
/// must never put pressure on the hot path.
#[derive(Default, Debug)]
pub struct RpcStats {
    /// JSON-RPC method calls issued (batch elements count individually).
    pub calls: AtomicU64,
    /// HTTP round trips attempted.
    pub requests: AtomicU64,
    /// Round trips that produced a usable protocol response.
    pub ok: AtomicU64,
    /// Round trips that failed for any reason (transport, HTTP, protocol).
    pub errors: AtomicU64,
    /// Rejections for load-shedding: HTTP 429 or a provider rate-limit
    /// JSON-RPC error (public Base endpoints emit -32016 "over rate limit").
    pub rate_limited: AtomicU64,
    /// Sum of successful round-trip latencies in ms (divide by `ok`).
    pub total_latency_ms: AtomicU64,
    /// Wall clock of the last successful round trip (unix ms); 0 = never.
    pub last_ok_ms: AtomicU64,
    /// Wall clock of the last failed round trip (unix ms); 0 = never.
    pub last_error_ms: AtomicU64,
}

impl RpcStats {
    pub fn snapshot(&self) -> serde_json::Value {
        use std::sync::atomic::Ordering::Relaxed;
        let ok = self.ok.load(Relaxed);
        let errors = self.errors.load(Relaxed);
        let total = ok + errors;
        serde_json::json!({
            "calls": self.calls.load(Relaxed),
            "requests": self.requests.load(Relaxed),
            "ok": ok,
            "errors": errors,
            "errorRateBps": errors
                .saturating_mul(10_000)
                .checked_div(total)
                .unwrap_or(0),
            "rateLimited": self.rate_limited.load(Relaxed),
            "avgLatencyMs": self
                .total_latency_ms
                .load(Relaxed)
                .checked_div(ok)
                .unwrap_or(0),
            "lastOkMs": self.last_ok_ms.load(Relaxed),
            "lastErrorMs": self.last_error_ms.load(Relaxed),
        })
    }
}

impl RpcClient {
    pub fn new(url: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .pool_max_idle_per_host(32)
            .build()?;
        Ok(Self {
            http,
            url: url.into(),
            id: std::sync::Arc::new(AtomicU64::new(1)),
            headers: Vec::new(),
            stats: std::sync::Arc::new(RpcStats::default()),
        })
    }

    /// The shared data-plane health counters for this endpoint.
    pub fn stats(&self) -> &RpcStats {
        &self.stats
    }

    fn record_request(&self, methods: u64) {
        self.stats.calls.fetch_add(methods, Ordering::Relaxed);
        self.stats.requests.fetch_add(1, Ordering::Relaxed);
    }

    fn record_round_trip(&self, elapsed: std::time::Duration, ok: bool) {
        if ok {
            self.stats.ok.fetch_add(1, Ordering::Relaxed);
            self.stats
                .total_latency_ms
                .fetch_add(elapsed.as_millis() as u64, Ordering::Relaxed);
            self.stats
                .last_ok_ms
                .store(crate::types::now_ms(), Ordering::Relaxed);
        } else {
            self.stats.errors.fetch_add(1, Ordering::Relaxed);
            self.stats
                .last_error_ms
                .store(crate::types::now_ms(), Ordering::Relaxed);
        }
    }

    /// HTTP 429 or provider `-32016 "over rate limit"`.
    fn note_rate_limited(&self) {
        self.stats.rate_limited.fetch_add(1, Ordering::Relaxed);
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn with_header(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }

    fn next_id(&self) -> u64 {
        self.id.fetch_add(1, Ordering::Relaxed)
    }

    /// Perform a single JSON-RPC call and deserialize the `result` field.
    pub async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let raw = self.call_raw(method, params).await?;
        serde_json::from_value(raw).with_context(|| format!("decoding result of {method}"))
    }

    pub async fn call_raw(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": method,
            "params": params,
        });
        self.send(body).await
    }

    pub async fn eth_call(&self, to: Address, data: Vec<u8>, block_number: u64) -> Result<Vec<u8>> {
        let tag = if block_number == 0 {
            "latest".to_string()
        } else {
            format!("0x{block_number:x}")
        };
        let params = json!([
            {
                "to": format!("{to:?}"),
                "data": format!("0x{}", hex::encode(data)),
            },
            tag
        ]);
        let res = self.call_raw("eth_call", params).await?;
        let hex_str = res
            .as_str()
            .ok_or_else(|| anyhow!("eth_call result not string"))?;
        let clean = hex_str.strip_prefix("0x").unwrap_or(hex_str);
        hex::decode(clean).map_err(|e| anyhow!("hex decode error: {e}"))
    }

    pub async fn get_transaction_count(&self, address: Address, block_number: u64) -> Result<u64> {
        let tag = if block_number == 0 {
            "latest".to_string()
        } else {
            format!("0x{block_number:x}")
        };
        let params = json!([format!("{address:?}"), tag]);
        let res = self.call_raw("eth_getTransactionCount", params).await?;
        let hex_str = res
            .as_str()
            .ok_or_else(|| anyhow!("getTransactionCount result not string"))?;
        let clean = hex_str.strip_prefix("0x").unwrap_or(hex_str);
        u64::from_str_radix(clean, 16).map_err(|e| anyhow!("parse u64 error: {e}"))
    }

    /// Like [`RpcClient::call_raw`], but surfaces the JSON-RPC **error
    /// object** (code/message/data) instead of flattening it into a string.
    ///
    /// The simulator needs this to read revert *data* out of an `eth_call`
    /// that reverted: anvil returns the raw revert bytes (custom-error
    /// selector and args) in the error's `data` field, which is exactly what
    /// turns "tx 0x… reverted" into "back-run reverted:
    /// Unprofitable(realised=0, required=1)".
    pub async fn call_raw_with_error(&self, method: &str, params: Value) -> Result<Value, Value> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": method,
            "params": params,
        });
        let mut req = self
            .http
            .post(&self.url)
            .header("content-type", "application/json");
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let resp = match req.json(&body).send().await {
            Ok(r) => r,
            Err(e) => return Err(json!({"code": -1, "message": e.to_string()})),
        };
        let status = resp.status();
        let text = match resp.text().await {
            Ok(t) => t,
            Err(e) => return Err(json!({"code": -1, "message": e.to_string()})),
        };
        if !status.is_success() {
            return Err(
                json!({"code": -1, "message": format!("http {status}: {}", truncate(&text, 512))}),
            );
        }
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => return Err(json!({"code": -1, "message": truncate(&text, 512)})),
        };
        if let Some(err) = v.get("error") {
            return Err(err.clone());
        }
        v.get("result")
            .cloned()
            .ok_or_else(|| json!({"code": -1, "message": truncate(&text, 512)}))
    }

    /// Same as [`call_raw`] but signs the payload with the Flashbots searcher key.
    pub async fn call_signed(
        &self,
        method: &str,
        params: Value,
        signer: &crate::signer::Signer,
    ) -> Result<Value> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": method,
            "params": params,
        });
        let text = serde_json::to_string(&body)?;
        let header = signer.flashbots_header(text.as_bytes());
        self.record_request(1);
        let started = std::time::Instant::now();
        let result = async {
            let resp = self
                .http
                .post(&self.url)
                .header("content-type", "application/json")
                .header("X-Flashbots-Signature", header)
                .body(text)
                .send()
                .await?;
            self.decode(resp).await
        }
        .await;
        self.record_round_trip(started.elapsed(), result.is_ok());
        result
    }

    async fn send(&self, body: Value) -> Result<Value> {
        self.record_request(1);
        let mut req = self
            .http
            .post(&self.url)
            .header("content-type", "application/json");
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let started = std::time::Instant::now();
        let result = async {
            let resp = req.json(&body).send().await?;
            self.decode(resp).await
        }
        .await;
        self.record_round_trip(started.elapsed(), result.is_ok());
        result
    }

    async fn decode(&self, resp: reqwest::Response) -> Result<Value> {
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            if status.as_u16() == 429 {
                self.note_rate_limited();
            }
            bail!("rpc http {status}: {}", truncate(&text, 512));
        }
        let v: Value = serde_json::from_str(&text)
            .with_context(|| format!("rpc returned non-json: {}", truncate(&text, 512)))?;
        if let Some(err) = v.get("error") {
            // Public endpoints signal load-shedding as a JSON-RPC error too
            // (e.g. -32016 "over rate limit"), not only as HTTP 429.
            let msg = err["message"].as_str().unwrap_or_default();
            if err["code"].as_i64() == Some(-32016) || msg.contains("rate limit") {
                self.note_rate_limited();
            }
            bail!("rpc error: {err}");
        }
        v.get("result")
            .cloned()
            .ok_or_else(|| anyhow!("rpc response has no result: {}", truncate(&text, 512)))
    }

    /// Batch several calls into one round trip. Returns results in request order;
    /// individual failures come back as `Err`.
    pub async fn batch(&self, calls: &[(String, Value)]) -> Result<Vec<Result<Value>>> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }
        self.record_request(calls.len() as u64);
        let mut payload = Vec::with_capacity(calls.len());
        let mut ids = Vec::with_capacity(calls.len());
        for (method, params) in calls {
            let id = self.next_id();
            ids.push(id);
            payload.push(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        }
        let mut req = self
            .http
            .post(&self.url)
            .header("content-type", "application/json");
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let started = std::time::Instant::now();
        let result = self.batch_inner(req, payload, ids).await;
        self.record_round_trip(started.elapsed(), result.is_ok());
        result
    }

    async fn batch_inner(
        &self,
        req: reqwest::RequestBuilder,
        payload: Vec<Value>,
        ids: Vec<u64>,
    ) -> Result<Vec<Result<Value>>> {
        let resp = req.json(&Value::Array(payload)).send().await?;
        if resp.status().as_u16() == 429 {
            self.note_rate_limited();
        }
        let text = resp.text().await?;
        let parsed: Value = serde_json::from_str(&text)
            .with_context(|| format!("batch returned non-json: {}", truncate(&text, 512)))?;
        let arr = parsed
            .as_array()
            .ok_or_else(|| anyhow!("batch response is not an array: {}", truncate(&text, 512)))?;

        let mut out: Vec<Result<Value>> = Vec::with_capacity(ids.len());
        for id in &ids {
            let entry = arr
                .iter()
                .find(|e| e.get("id").and_then(|v| v.as_u64()) == Some(*id));
            match entry {
                Some(e) => {
                    if let Some(err) = e.get("error") {
                        // Batch elements can be individually rate-limited.
                        let msg = err["message"].as_str().unwrap_or_default();
                        if err["code"].as_i64() == Some(-32016) || msg.contains("rate limit") {
                            self.note_rate_limited();
                        }
                        out.push(Err(anyhow!("rpc error: {err}")));
                    } else {
                        out.push(Ok(e.get("result").cloned().unwrap_or(Value::Null)));
                    }
                }
                None => out.push(Err(anyhow!("missing response for id {id}"))),
            }
        }
        Ok(out)
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

/// A websocket `eth_subscribe` stream, reconnecting on failure.
///
/// Yields the raw `params.result` value of each notification.
pub struct WsSubscription {
    pub rx: mpsc::Receiver<Value>,
}

impl WsSubscription {
    /// Spawn a task that keeps `eth_subscribe(<args>)` alive forever.
    pub fn spawn(url: String, subscribe_params: Value, label: &'static str) -> Self {
        Self::spawn_observed(url, subscribe_params, label, None)
    }

    /// `spawn` with an optional reconnect counter: bumped once per lost
    /// connection, after the first successful (re)subscribe. Feeds whose
    /// state can silently gap during a reconnect (Flashblocks) need the
    /// count to distinguish "no frames" from "feed broke".
    pub fn spawn_observed(
        url: String,
        subscribe_params: Value,
        label: &'static str,
        reconnects: Option<Arc<std::sync::atomic::AtomicU64>>,
    ) -> Self {
        let (tx, rx) = mpsc::channel(4096);
        tokio::spawn(async move {
            let mut backoff_ms = 500u64;
            let mut first = true;
            loop {
                match run_subscription(&url, &subscribe_params, &tx).await {
                    Ok(()) => {
                        tracing::warn!(target: "ingest", %label, "subscription closed, reconnecting");
                    }
                    Err(e) => {
                        tracing::warn!(target: "ingest", %label, error = %e, "subscription failed");
                    }
                }
                if first {
                    first = false;
                } else if let Some(c) = &reconnects {
                    c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(15_000);
                if tx.is_closed() {
                    return;
                }
            }
        });
        Self { rx }
    }
}

async fn run_subscription(url: &str, params: &Value, tx: &mpsc::Sender<Value>) -> Result<()> {
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await?;
    let sub = json!({"jsonrpc":"2.0","id":1,"method":"eth_subscribe","params":params});
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        sub.to_string(),
    ))
    .await?;

    while let Some(msg) = ws.next().await {
        let msg = msg?;
        let text = match msg {
            tokio_tungstenite::tungstenite::Message::Text(t) => t,
            tokio_tungstenite::tungstenite::Message::Binary(b) => {
                String::from_utf8_lossy(&b).to_string()
            }
            tokio_tungstenite::tungstenite::Message::Ping(p) => {
                ws.send(tokio_tungstenite::tungstenite::Message::Pong(p))
                    .await?;
                continue;
            }
            tokio_tungstenite::tungstenite::Message::Close(_) => break,
            _ => continue,
        };
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(result) = v.pointer("/params/result") {
            if tx.send(result.clone()).await.is_err() {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Consume a Server-Sent Events endpoint, yielding each `data:` payload.
pub fn sse_stream(url: String, label: &'static str) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel(2048);
    tokio::spawn(async move {
        // No global timeout: an SSE connection is expected to stay open forever.
        let client = match reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(target: "ingest", %label, error = %e, "sse client build failed");
                return;
            }
        };
        let mut backoff_ms = 500u64;
        loop {
            match run_sse(&client, &url, &tx).await {
                Ok(()) => {
                    tracing::warn!(target: "ingest", %label, "sse stream ended, reconnecting")
                }
                Err(e) => tracing::warn!(target: "ingest", %label, error = %e, "sse stream failed"),
            }
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(15_000);
            if tx.is_closed() {
                return;
            }
        }
    });
    rx
}

async fn run_sse(client: &reqwest::Client, url: &str, tx: &mpsc::Sender<String>) -> Result<()> {
    let resp = client
        .get(url)
        .header("accept", "text/event-stream")
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("sse http {}", resp.status());
    }
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        // Events are separated by a blank line; fields we care about start with "data:".
        while let Some(idx) = buf.find("\n\n") {
            let event: String = buf.drain(..idx + 2).collect();
            for line in event.lines() {
                if let Some(data) = line.strip_prefix("data:") {
                    let data = data.trim();
                    if data.is_empty() || data == ":ping" {
                        continue;
                    }
                    if tx.send(data.to_string()).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
        if buf.len() > 1 << 20 {
            buf.clear(); // pathological producer; drop the buffer rather than grow forever
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    #[test]
    fn truncate_is_utf8_safe_for_ascii() {
        assert_eq!(super::truncate("hello", 10), "hello");
        assert_eq!(super::truncate("hello", 2), "he…");
    }

    /// A one-shot JSON-RPC stub answering `respond(body)` with a canned
    /// response (or raw HTTP status).
    async fn mock_server(
        respond: impl Fn(&Value) -> (u16, Value) + Send + Sync + 'static,
    ) -> String {
        // axum handlers must be Clone: the responder is shared across
        // requests behind an Arc rather than moved into one closure.
        let respond = std::sync::Arc::new(respond);
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(
                move |axum::extract::Json(req): axum::extract::Json<Value>| {
                    let respond = respond.clone();
                    async move {
                        let (status, body) = respond(&req);
                        (
                            axum::http::StatusCode::from_u16(status).expect("status code"),
                            axum::Json(body),
                        )
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn rpc_stats_count_ok_errors_and_rate_limits_honestly() {
        use super::RpcClient;
        // 1. Success path: ok + latency land, no errors.
        let url = mock_server(|req| {
            (
                200,
                json!({"jsonrpc": "2.0", "id": req["id"], "result": "0x2105"}),
            )
        })
        .await;
        let rpc = RpcClient::new(url).unwrap();
        let out: String = rpc.call("eth_chainId", json!([])).await.unwrap();
        assert_eq!(out, "0x2105");
        assert_eq!(
            rpc.stats().calls.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(rpc.stats().ok.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(
            rpc.stats()
                .errors
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert!(
            rpc.stats()
                .last_ok_ms
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0
        );

        // 2. HTTP 429 → errors + rateLimited, never counted as ok.
        let url =
            mock_server(|_| (429, json!({"error": "max project request rate exceeded"}))).await;
        let rpc = RpcClient::new(url).unwrap();
        assert!(rpc.call::<String>("eth_chainId", json!([])).await.is_err());
        assert_eq!(
            rpc.stats()
                .rate_limited
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(
            rpc.stats()
                .errors
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(rpc.stats().ok.load(std::sync::atomic::Ordering::Relaxed), 0);

        // 3. Provider JSON-RPC rate-limit error (-32016) → also load-shedding.
        let url = mock_server(|req| {
            (
                200,
                json!({
                    "jsonrpc": "2.0", "id": req["id"],
                    "error": {"code": -32016, "message": "over rate limit"}
                }),
            )
        })
        .await;
        let rpc = RpcClient::new(url).unwrap();
        assert!(rpc.call::<String>("eth_chainId", json!([])).await.is_err());
        assert_eq!(
            rpc.stats()
                .rate_limited
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(
            rpc.stats()
                .errors
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );

        // 4. Batch counts each method as a call on one round trip.
        let url = mock_server(|req| {
            let arr = req.as_array().unwrap();
            let out: Vec<Value> = arr
                .iter()
                .map(|one| json!({"jsonrpc": "2.0", "id": one["id"], "result": "0x1"}))
                .collect();
            (200, Value::Array(out))
        })
        .await;
        let rpc = RpcClient::new(url).unwrap();
        let res = rpc
            .batch(&[
                ("eth_chainId".to_string(), json!([])),
                ("eth_blockNumber".to_string(), json!([])),
            ])
            .await
            .unwrap();
        assert!(res.iter().all(|r| r.is_ok()));
        assert_eq!(
            rpc.stats().calls.load(std::sync::atomic::Ordering::Relaxed),
            2
        );
        assert_eq!(
            rpc.stats()
                .requests
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn the_snapshot_is_the_console_health_contract() {
        use super::RpcClient;
        // One healthy endpoint: two single calls, both succeed.
        let ok_url = mock_server(|req| {
            (
                200,
                json!({"jsonrpc": "2.0", "id": req["id"], "result": "0x1"}),
            )
        })
        .await;
        let rpc = RpcClient::new(ok_url).unwrap();
        rpc.call::<String>("eth_blockNumber", json!([]))
            .await
            .unwrap();
        rpc.call::<String>("eth_chainId", json!([])).await.unwrap();

        let before_err_ms = crate::types::now_ms();
        // An endpoint that always load-sheds: every call is an error, and
        // exactly the classified one.
        let limited_url = mock_server(|req| {
            (
                429,
                json!({"jsonrpc": "2.0", "id": req["id"],
                       "error": {"code": -32016, "message": "over rate limit"}}),
            )
        })
        .await;
        let limited = RpcClient::new(limited_url).unwrap();
        assert!(limited
            .call::<String>("eth_blockNumber", json!([]))
            .await
            .is_err());

        let snap = rpc.stats().snapshot();
        assert_eq!(snap["calls"], 2);
        assert_eq!(snap["requests"], 2);
        assert_eq!(snap["ok"], 2);
        assert_eq!(snap["errors"], 0);
        assert_eq!(snap["errorRateBps"], 0);
        // lastOkMs is wall-clock ms, 0 means "never" — after successes it
        // must be a sane timestamp, not the sentinel.
        let last_ok = snap["lastOkMs"].as_u64().unwrap();
        assert!(last_ok >= before_err_ms.saturating_sub(5_000));
        assert_eq!(snap["lastErrorMs"], 0);

        let snap = limited.stats().snapshot();
        assert_eq!(snap["ok"], 0);
        assert_eq!(snap["errors"], 1);
        assert_eq!(snap["rateLimited"], 1, "429 must classify as rate limiting");
        assert_eq!(snap["errorRateBps"], 10_000, "every request failed");
        assert_eq!(
            snap["avgLatencyMs"], 0,
            "no successful round trips to average"
        );
        assert_eq!(snap["lastOkMs"], 0, "never succeeded");
        assert!(snap["lastErrorMs"].as_u64().unwrap() >= before_err_ms);
    }
}
