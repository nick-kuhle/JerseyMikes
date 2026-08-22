"use client";

import {memo, useMemo} from "react";
import {LineChart, Line, XAxis, YAxis, Tooltip, ResponsiveContainer, ReferenceLine, CartesianGrid} from "recharts";
import type {SeriesPoint} from "@/lib/types";

/** Cumulative simulated PnL, in ETH, bucketed by target block. */
function EquityChart({series}: {series: SeriesPoint[]}) {
  // Up to 250 points reduced on every render otherwise — and recharts then
  // re-renders the whole SVG because `data` is a new array identity.
  const data = useMemo(() => {
    let cum = 0;
    return series.map((p) => {
      cum += p.netWei;
      return {block: p.block, eth: cum / 1e18, blockNet: p.netWei / 1e18, count: p.count};
    });
  }, [series]);

  const last = data.length ? data[data.length - 1].eth : 0;
  const color = last >= 0 ? "#35d07f" : "#ff5c5c";

  if (!data.length) {
    return <div className="muted" style={{padding: 24, textAlign: "center"}}>no simulations yet</div>;
  }

  return (
    <div style={{height: 220, padding: "12px 8px 0 0"}}>
      <ResponsiveContainer width="100%" height="100%">
        <LineChart data={data} margin={{top: 4, right: 12, bottom: 4, left: 4}}>
          <CartesianGrid stroke="#141c26" vertical={false} />
          <XAxis
            dataKey="block"
            tick={{fill: "#6b7c93", fontSize: 10}}
            tickLine={false}
            axisLine={{stroke: "#1b2532"}}
            minTickGap={40}
          />
          <YAxis
            tick={{fill: "#6b7c93", fontSize: 10}}
            tickLine={false}
            axisLine={false}
            width={58}
            tickFormatter={(v: number) => v.toFixed(3)}
          />
          <Tooltip
            contentStyle={{
              background: "#0b1017",
              border: "1px solid #1b2532",
              borderRadius: 4,
              fontSize: 11,
              fontFamily: "ui-monospace, monospace",
            }}
            labelStyle={{color: "#6b7c93"}}
            formatter={(value: number, name: string) => [
              `${value >= 0 ? "+" : ""}${value.toFixed(6)} ETH`,
              name === "eth" ? "cumulative" : "block net",
            ]}
          />
          <ReferenceLine y={0} stroke="#2a3646" strokeDasharray="3 3" />
          <Line type="monotone" dataKey="eth" stroke={color} strokeWidth={1.6} dot={false} isAnimationActive={false} />
        </LineChart>
      </ResponsiveContainer>
    </div>
  );
}

/**
 * Memoized: an SVG chart is one of the most expensive things on the page, and
 * its input only changes on the 4s status poll — never on an SSE flush.
 */
export default memo(EquityChart);
