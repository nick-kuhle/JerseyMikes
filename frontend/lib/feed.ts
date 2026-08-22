"use client";

/**
 * SSE feed consumption: typed parsing + batched delivery.
 *
 * The bot's live tape is bursty. A busy mainnet block delivers a few hundred
 * pending transactions, relay blocks and simulations within a second or two,
 * and the previous consumer called `setEvents` once per frame — one React
 * render per event. At 200 events/s that is 200 renders/s of a table that can
 * only paint at 60 Hz, so the browser spends its budget on work nobody sees.
 *
 * Two fixes live here:
 *
 * 1. **Batching.** Frames are accumulated in a plain array and flushed on a
 *    timer (`FLUSH_MS`). One flush = one state update = one render, however
 *    many events arrived in between. The tape is a log, not a control surface,
 *    so a fixed sub-perceptual delay costs the user nothing.
 * 2. **Typed parsing.** `JSON.parse(...) as FeedEvent` is a lie the compiler
 *    believes: a malformed frame becomes an object with a missing `kind`, and
 *    the renderer crashes on a field it was promised existed. `parseFeedEvent`
 *    validates the discriminant and the fields each variant actually reads,
 *    and returns `null` for anything else.
 */

import {useEffect, useRef, useState} from "react";
import type {FeedEvent, RelayTxSummary, Strategy} from "./types";

/**
 * How long events accumulate before a flush.
 *
 * ~8 renders/s: below the threshold where a log feels laggy, far below the
 * rate an unbatched feed would demand. Bursts collapse into one render.
 */
export const FLUSH_MS = 120;

/* ── Typed parsing ────────────────────────────────────────────────────────── */

type Json = Record<string, unknown>;

const isObj = (v: unknown): v is Json => typeof v === "object" && v !== null && !Array.isArray(v);
const str = (v: unknown): v is string => typeof v === "string";
const num = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);
const bool = (v: unknown): v is boolean => typeof v === "boolean";
/** Nullable string: the bot sends `null` for absent addresses/selectors. */
const strOrNull = (v: unknown): v is string | null => v === null || typeof v === "string";

function relayTxSummaries(v: unknown): RelayTxSummary[] {
  if (!Array.isArray(v)) return [];
  return v.filter(
    (t): t is RelayTxSummary => isObj(t) && str(t.hash),
  );
}

/**
 * Validate one decoded SSE frame.
 *
 * Returns `null` when the frame is not a `FeedEvent` this UI can render, so a
 * protocol change or a truncated frame degrades into a dropped row instead of
 * a runtime crash inside the table.
 */
export function parseFeedEvent(raw: unknown): FeedEvent | null {
  if (!isObj(raw) || !str(raw.kind)) return null;

  switch (raw.kind) {
    case "block":
      if (!num(raw.number) || !str(raw.hash)) return null;
      return {
        kind: "block",
        number: raw.number,
        hash: raw.hash,
        base_fee_per_gas: str(raw.base_fee_per_gas) ? raw.base_fee_per_gas : "0",
        gas_used: num(raw.gas_used) ? raw.gas_used : 0,
        timestamp: num(raw.timestamp) ? raw.timestamp : 0,
      };

    case "pending":
      if (!str(raw.hash)) return null;
      return {
        kind: "pending",
        hash: raw.hash,
        from: strOrNull(raw.from) ? raw.from : null,
        to: strOrNull(raw.to) ? raw.to : null,
        value: str(raw.value) ? raw.value : "0",
        gas: num(raw.gas) ? raw.gas : 0,
        source: str(raw.source) ? raw.source : "unknown",
        selector: strOrNull(raw.selector) ? raw.selector : null,
        seen_at_ms: num(raw.seen_at_ms) ? raw.seen_at_ms : 0,
      };

    case "mev_share_hint":
      if (!str(raw.hash)) return null;
      return {
        kind: "mev_share_hint",
        hash: raw.hash,
        logs: num(raw.logs) ? raw.logs : 0,
        functions: Array.isArray(raw.functions) ? raw.functions.filter(str) : [],
        seen_at_ms: num(raw.seen_at_ms) ? raw.seen_at_ms : 0,
      };

    case "opportunity":
      if (!str(raw.strategy)) return null;
      return {
        kind: "opportunity",
        id: str(raw.id) ? raw.id : "",
        strategy: raw.strategy as Strategy,
        notes: str(raw.notes) ? raw.notes : "",
        expected_profit_wei: str(raw.expected_profit_wei) ? raw.expected_profit_wei : "0",
        target_block: num(raw.target_block) ? raw.target_block : 0,
      };

    case "simulation":
      if (!str(raw.strategy)) return null;
      return {
        kind: "simulation",
        opportunity_id: str(raw.opportunity_id) ? raw.opportunity_id : "",
        strategy: raw.strategy as Strategy,
        backend: str(raw.backend) ? raw.backend : "",
        success: bool(raw.success) ? raw.success : false,
        // Signed and can legitimately be negative — a losing simulation is a
        // result, not an error.
        net_profit_wei: str(raw.net_profit_wei)
          ? raw.net_profit_wei
          : num(raw.net_profit_wei)
            ? String(raw.net_profit_wei)
            : "0",
        gas_used: num(raw.gas_used) ? raw.gas_used : 0,
        gross_profit_wei: str(raw.gross_profit_wei) ? raw.gross_profit_wei : "0",
        revert_reason: strOrNull(raw.revert_reason) ? raw.revert_reason : null,
      };

    case "bundle":
      if (!str(raw.strategy)) return null;
      return {
        kind: "bundle",
        id: str(raw.id) ? raw.id : "",
        strategy: raw.strategy as Strategy,
        target_block: num(raw.target_block) ? raw.target_block : 0,
        submitted: bool(raw.submitted) ? raw.submitted : false,
      };

    case "relay":
      if (!str(raw.relay)) return null;
      return {
        kind: "relay",
        relay: raw.relay,
        slot: num(raw.slot) ? raw.slot : 0,
        builder: str(raw.builder) ? raw.builder : "",
        value_wei: str(raw.value_wei) ? raw.value_wei : "0",
        seen_at_ms: num(raw.seen_at_ms) ? raw.seen_at_ms : 0,
      };

    case "relay_block": {
      // `block.block_number` is read unconditionally by the renderer.
      if (!isObj(raw.block) || !num((raw.block as Json).block_number)) return null;
      return {
        kind: "relay_block",
        block: raw.block as FeedEvent extends {kind: "relay_block"; block: infer B} ? B : never,
        tx_count: num(raw.tx_count) ? raw.tx_count : 0,
        txs: relayTxSummaries(raw.txs),
      };
    }

    case "alert":
      return {
        kind: "alert",
        rule: str(raw.rule) ? raw.rule : "",
        severity: str(raw.severity) ? raw.severity : "info",
        message: str(raw.message) ? raw.message : "",
        active: bool(raw.active) ? raw.active : false,
        seen_at_ms: num(raw.seen_at_ms) ? raw.seen_at_ms : 0,
      };

    case "reorg":
      if (!num(raw.from_block) || !num(raw.to_block)) return null;
      return {
        kind: "reorg",
        from_block: raw.from_block,
        to_block: raw.to_block,
        depth: num(raw.depth) ? raw.depth : 1,
        old_hash: str(raw.old_hash) ? raw.old_hash : "",
        new_hash: str(raw.new_hash) ? raw.new_hash : "",
        seen_at_ms: num(raw.seen_at_ms) ? raw.seen_at_ms : 0,
      };

    default:
      // Unknown kind: a newer bot emitting an event this UI predates.
      return null;
  }
}

/** Decode one raw SSE `data:` payload. Never throws. */
export function decodeFrame(data: string): FeedEvent | null {
  try {
    return parseFeedEvent(JSON.parse(data));
  } catch {
    return null;
  }
}

/* ── Batched SSE hook ─────────────────────────────────────────────────────── */

export interface FeedState {
  events: FeedEvent[];
  connected: boolean;
  /** Events received since mount — including ones aged out of the buffer. */
  received: number;
}

/**
 * Subscribe to the bot's SSE tape, newest first, capped at `max`.
 *
 * Incoming frames land in a ref (no render), and a timer flushes the pending
 * batch into state. The flush is skipped entirely when nothing arrived, so an
 * idle feed costs nothing.
 */
export function useFeed(url: string, max: number): FeedState {
  const [events, setEvents] = useState<FeedEvent[]>([]);
  const [connected, setConnected] = useState(false);
  const [received, setReceived] = useState(0);

  // Buffer of frames not yet flushed into state.
  const pending = useRef<FeedEvent[]>([]);
  // Counted separately so the flush can bump both in one render.
  const receivedSinceFlush = useRef(0);

  useEffect(() => {
    const es = new EventSource(url);
    es.onopen = () => setConnected(true);
    es.onerror = () => setConnected(false);
    es.onmessage = (m: MessageEvent<string>) => {
      const ev = decodeFrame(m.data);
      if (ev) pending.current.push(ev);
    };

    const flush = setInterval(() => {
      const batch = pending.current;
      if (batch.length === 0) return;
      pending.current = [];
      receivedSinceFlush.current += batch.length;
      const got = receivedSinceFlush.current;
      receivedSinceFlush.current = 0;
      // One reversal + one slice per batch, not per event.
      setEvents((prev) => {
        const next = batch.length >= max ? batch.slice(-max).reverse() : [...batch.reverse(), ...prev];
        return next.length > max ? next.slice(0, max) : next;
      });
      setReceived((n) => n + got);
    }, FLUSH_MS);

    return () => {
      clearInterval(flush);
      es.close();
      pending.current = [];
      receivedSinceFlush.current = 0;
    };
  }, [url, max]);

  return {events, connected, received};
}
