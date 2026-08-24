import {NextRequest} from "next/server";
import {GET as botStream} from "@/app/api/bot/[...path]/route";

export const dynamic = "force-dynamic";

/**
 * Public same-origin stream surface. The existing /api/bot/stream route is
 * kept for backwards compatibility; this canonical alias makes the chain
 * contract explicit: /api/stream?chain=ethereum|base.
 *
 * The implementation is delegated to the authenticated server-side bot proxy,
 * so bot URLs and bearer tokens never reach the browser.
 */
export async function GET(req: NextRequest) {
  return botStream(req, {params: Promise.resolve({path: ["stream"]})});
}
