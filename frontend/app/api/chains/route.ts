import {chains} from "@/lib/chains";

export const dynamic = "force-dynamic";

/**
 * The chain registry for the browser: slugs + labels so the header switcher
 * can render the pills. Bot URLs stay server-side (private-network bots must
 * not leak into the browser bundle).
 */
export async function GET() {
  const list = chains();
  return Response.json({
    chains: list.map(({slug, label}) => ({slug, label})),
    default: list[0].slug,
  });
}
