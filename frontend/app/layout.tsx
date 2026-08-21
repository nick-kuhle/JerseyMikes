import type {Metadata} from "next";
import "./globals.css";
import {WalletProvider} from "@/lib/wallet";

export const metadata: Metadata = {
  title: "JerseyMikes — MEV simulation console",
  description: "Simulation-only MEV searcher: sandwich, JIT, atomic arb, liquidation, sniper",
};

export default function RootLayout({children}: {children: React.ReactNode}) {
  return (
    <html lang="en">
      <body>
        <WalletProvider>{children}</WalletProvider>
      </body>
    </html>
  );
}
