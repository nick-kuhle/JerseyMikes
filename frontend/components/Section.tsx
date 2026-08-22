"use client";

import {useState, type ReactNode} from "react";

/**
 * Collapsible console section with a stable anchor for the jump nav.
 *
 * The console grew long enough that scrolling the page meant scrolling
 * *through* whatever tall table the cursor happened to be over. Sections
 * collapse instead: the heavy tables are one click away, the page between
 * them is short, and the sticky nav jumps straight to what is needed.
 */
export default function Section({
  id,
  title,
  subtitle,
  defaultOpen = true,
  children,
}: {
  /** Anchor id — the jump nav links to `#id`. */
  id: string;
  title: string;
  subtitle?: string;
  defaultOpen?: boolean;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <section className="panel" id={id} style={{scrollMarginTop: 56}}>
      <div
        className="panel-head"
        style={{cursor: "pointer", userSelect: "none"}}
        onClick={() => setOpen((o) => !o)}
        title={open ? "collapse section" : "expand section"}
      >
        <span>
          <span
            style={{
              display: "inline-block",
              width: 14,
              color: "var(--muted)",
              fontSize: 10,
              transform: open ? "none" : "rotate(-90deg)",
              transition: "transform 120ms ease",
            }}
          >
            ▾
          </span>
          {title}
        </span>
        {subtitle && <span className="muted">{subtitle}</span>}
      </div>
      {open && <div style={{paddingTop: 4}}>{children}</div>}
    </section>
  );
}
