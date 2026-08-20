/** @type {import('next').NextConfig} */
const nextConfig = {
  reactStrictMode: true,
  // The dev server is reached through a proxied preview host, so it must accept
  // requests whose Origin/Host is not localhost.
  allowedDevOrigins: ["*.e2b.app", "*.e2b.dev", "localhost", "127.0.0.1"],
  eslint: {ignoreDuringBuilds: true},
};

export default nextConfig;
