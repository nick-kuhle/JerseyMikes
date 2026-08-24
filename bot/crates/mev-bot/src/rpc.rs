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
        })
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
        let resp = self
            .http
            .post(&self.url)
            .header("content-type", "application/json")
            .header("X-Flashbots-Signature", header)
            .body(text)
            .send()
            .await?;
        Self::decode(resp).await
    }

    async fn send(&self, body: Value) -> Result<Value> {
        let mut req = self
            .http
            .post(&self.url)
            .header("content-type", "application/json");
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let resp = req.json(&body).send().await?;
        Self::decode(resp).await
    }

    async fn decode(resp: reqwest::Response) -> Result<Value> {
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            bail!("rpc http {status}: {}", truncate(&text, 512));
        }
        let v: Value = serde_json::from_str(&text)
            .with_context(|| format!("rpc returned non-json: {}", truncate(&text, 512)))?;
        if let Some(err) = v.get("error") {
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
        let resp = req.json(&Value::Array(payload)).send().await?;
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
    #[test]
    fn truncate_is_utf8_safe_for_ascii() {
        assert_eq!(super::truncate("hello", 10), "hello");
        assert_eq!(super::truncate("hello", 2), "he…");
    }
}
