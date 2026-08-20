import type {Metadata} from "next";
import "./globals.css";

export const metadata: Metadata = {
  title: "JerseyMikes — MEV simulation console",
  description: "Simulation-only MEV searcher: sandwich, JIT, atomic arb, liquidation, sniper",
};

export default function RootLayout({children}: {children: React.ReactNode}) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
