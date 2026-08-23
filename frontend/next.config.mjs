/** @type {import('next').NextConfig} */
const nextConfig = {
  reactStrictMode: true,
  // `frontend/lib/MevExecutor.creation.hex` (a byte-for-byte copy of the
  // bot artifact, drift-checked by CI) is imported as a string by the
  // go-live deploy panel.
  webpack: (config) => {
    config.module.rules.push({test: /\.hex$/, type: "asset/source"});
    return config;
  },
  // The dev server is reached through a proxied preview host, so it must accept
  // requests whose Origin/Host is not localhost.
  allowedDevOrigins: ["*.e2b.app", "*.e2b.dev", "localhost", "127.0.0.1"],
};

export default nextConfig;
