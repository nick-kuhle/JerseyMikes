"use client";

/**
 * Execution-mode switch: simulation ⇄ live.
 *
 * The bot's mode has two layers (`docs/RISK.md`):
 *   1. **Boot-time arming** — `LIVE_EXECUTION=true` + `I_UNDERSTAND_LIVE_RISK=yes`
 *      in the bot's environment, read once at startup. This API cannot grant
 *      it; a process that was not armed refuses the switch with a 409.
 *   2. **Runtime switch** — for an armed process, `POST /api/mode` flips
 *      simulation/live without a restart. It can only *narrow* what the
 *      environment allowed, never widen it.
 *
 * So the UI is honest about both: an unarmed bot shows the arming steps
 * instead of a toggle that would silently do nothing, and arming an armed bot
 * takes a typed confirmation. In demo mode (bot unreachable) the switch flips
 * only the demo state and says so.
 */

import {useState} from "react";
import type {ModeResponse} from "@/lib/types";

interface Props {
  mode: "simulation" | "live" | undefined;
  armed?: boolean;
  demo: boolean;
  /** Refetch `/api/status` after a successful (or demo) flip. */
  onChanged: () => void;
}

export default function ModeSwitch({mode, armed, demo, onChanged}: Props) {
  const [dialog, setDialog] = useState<"none" | "confirm-live" | "confirm-sim" | "arming">("none");
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);

  const live = mode === "live";

  const post = async (want: boolean) => {
    setBusy(true);
    setNote(null);
    try {
      const r = await fetch("/api/bot/mode", {
        method: "POST",
        headers: {"content-type": "application/json"},
        body: JSON.stringify({live: want}),
      });
      const j = (await r.json()) as ModeResponse;
      if (!r.ok || j.ok === false) {
        setNote(j.hint ?? j.error ?? `switch refused (HTTP ${r.status})`);
      } else {
        setNote(null);
        setDialog("none");
        setTyped("");
        onChanged();
      }
    } catch (e) {
      setNote((e as Error).message.split("\n")[0]);
    } finally {
      setBusy(false);
    }
  };

  const onToggle = () => {
    if (!mode) return; // status not loaded yet
    if (live) {
      setDialog("confirm-sim");
    } else if (!armed && !demo) {
      setDialog("arming");
    } else {
      setDialog("confirm-live");
      setTyped("");
    }
  };

  return (
    <>
      <span
        className="badge"
        style={{
          color: live ? "#ff5c5c" : "#35d07f",
          cursor: "pointer",
          userSelect: "none",
          border: "1px solid #24334a",
        }}
        onClick={onToggle}
        title={
          live
            ? "live execution is ON — click to pause back to simulation"
            : armed
              ? "simulation only — click to switch to live execution (typed confirmation)"
              : "simulation only — this bot was not started armed for live execution; click for the arming steps"
        }
      >
        <span className="dot" style={{background: live ? "#ff5c5c" : "#35d07f"}} />
        {live ? "LIVE EXECUTION" : "SIMULATION ONLY"}
        <span className="muted" style={{marginLeft: 6}}>
          ⇄
        </span>
      </span>

      {dialog !== "none" && (
        <Modal onClose={() => (busy ? undefined : setDialog("none"))}>
          {dialog === "arming" && <ArmingSteps onClose={() => setDialog("none")} />}
          {dialog === "confirm-sim" && (
            <div style={{display: "grid", gap: 10}}>
              <div style={{fontWeight: 700, color: "#35d07f"}}>Pause live execution?</div>
              <p className="muted" style={{margin: 0, fontSize: 12, lineHeight: 1.6}}>
                Profitable bundles will stop being marked submitted and the bot returns to
                record-only simulation. This takes effect immediately, no restart needed.
                {demo && " (demo mode — this flips the demo state only.)"}
              </p>
              <div style={{display: "flex", gap: 8}}>
                <button onClick={() => void post(false)} disabled={busy} style={primaryBtn}>
                  {busy ? "switching…" : "pause to simulation"}
                </button>
                <button onClick={() => setDialog("none")} disabled={busy} style={btn}>
                  cancel
                </button>
              </div>
            </div>
          )}
          {dialog === "confirm-live" && (
            <div style={{display: "grid", gap: 10}}>
              <div style={{fontWeight: 700, color: "#ff5c5c"}}>Switch to LIVE execution</div>
              <p className="muted" style={{margin: 0, fontSize: 12, lineHeight: 1.6}}>
                Profitable bundles will be marked submitted from this moment on. The executor&apos;s
                on-chain profit guard still reverts any unprofitable batch, and a reverting private
                bundle is dropped by the builder — but this is the switch that makes the bot act
                rather than observe. Type <b style={{color: "#ff5c5c"}}>LIVE</b> to confirm.
                {demo && (
                  <>
                    <br />
                    <b style={{color: "#f5b544"}}>Demo mode:</b> no bot is reachable, so this flips
                    the demo state only — nothing real can happen.
                  </>
                )}
              </p>
              <input
                value={typed}
                onChange={(e) => setTyped(e.target.value)}
                placeholder='type "LIVE"'
                spellCheck={false}
                style={inputStyle}
                disabled={busy}
              />
              {note && <div className="muted" style={{fontSize: 11, color: "#ff5c5c"}}>{note}</div>}
              <div style={{display: "flex", gap: 8}}>
                <button
                  onClick={() => void post(true)}
                  disabled={busy || typed.trim().toUpperCase() !== "LIVE"}
                  style={{...primaryBtn, borderColor: "#ff5c5c", color: "#ff5c5c"}}
                >
                  {busy ? "switching…" : "go live"}
                </button>
                <button onClick={() => setDialog("none")} disabled={busy} style={btn}>
                  cancel
                </button>
              </div>
            </div>
          )}
          {note && dialog === "arming" && (
            <div className="muted" style={{fontSize: 11, color: "#ff5c5c"}}>
              {note}
            </div>
          )}
        </Modal>
      )}
    </>
  );
}

function ArmingSteps({onClose}: {onClose: () => void}) {
  const [copied, setCopied] = useState(false);
  const snippet = `# bot/.env — restart required; the API can never arm a process itself
LIVE_EXECUTION=true
I_UNDERSTAND_LIVE_RISK=yes`;
  return (
    <div style={{display: "grid", gap: 10}}>
      <div style={{fontWeight: 700, color: "#f5b544"}}>Live execution is not armed</div>
      <p className="muted" style={{margin: 0, fontSize: 12, lineHeight: 1.6}}>
        The bot this dashboard talks to was started without live execution. Arming is
        deliberately boot-time only — <b>two</b> independent environment keys must be set by the
        operator before the process starts. Once the bot is restarted with them, the runtime
        switch on this dashboard can pause/resume live mode without another restart.
      </p>
      <pre
        style={{
          background: "#040608",
          border: "1px solid #1b2532",
          borderRadius: 4,
          padding: "8px 10px",
          fontSize: 11,
          color: "#a5b4fc",
          margin: 0,
          overflowX: "auto",
        }}
      >
        {snippet}
      </pre>
      <div style={{display: "flex", gap: 8}}>
        <button
          onClick={() => {
            void navigator.clipboard.writeText(snippet);
            setCopied(true);
            setTimeout(() => setCopied(false), 2000);
          }}
          style={primaryBtn}
        >
          {copied ? "copied ✓" : "copy env keys"}
        </button>
        <button onClick={onClose} style={btn}>
          close
        </button>
      </div>
      <div className="muted" style={{fontSize: 11}}>
        Also read <code>docs/RISK.md</code> — going live is Phase 3 of the roadmap and a separate
        decision.
      </div>
    </div>
  );
}

function Modal({children, onClose}: {children: React.ReactNode; onClose: () => void}) {
  return (
    <div
      onClick={onClose}
      style={{
        position: "fixed",
        inset: 0,
        background: "rgba(0,0,0,0.65)",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        zIndex: 100,
      }}
    >
      <div
        onClick={(e) => e.stopPropagation()}
        className="panel"
        style={{maxWidth: 520, width: "92vw", padding: 16}}
      >
        {children}
      </div>
    </div>
  );
}

const btn: React.CSSProperties = {
  background: "#111a25",
  border: "1px solid #24334a",
  borderRadius: 4,
  color: "#d7e2f0",
  padding: "6px 12px",
  cursor: "pointer",
  fontFamily: "inherit",
  fontSize: 12,
};

const primaryBtn: React.CSSProperties = {...btn, borderColor: "#22d3ee", color: "#22d3ee"};

const inputStyle: React.CSSProperties = {
  background: "#070b11",
  border: "1px solid #1b2532",
  borderRadius: 4,
  color: "#d7e2f0",
  padding: "6px 8px",
  fontFamily: "inherit",
  fontSize: 12,
};
