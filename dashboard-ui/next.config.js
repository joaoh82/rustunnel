/** @type {import('next').NextConfig} */
const nextConfig = {
  // Always produce a static export — deployed to Vercel as a standalone app.
  // Set NEXT_PUBLIC_API_URL in .env.local (dev) or as a Vercel env var (prod)
  // to point at the rustunnel-server dashboard API.
  output: 'export',
  images: { unoptimized: true },
};

module.exports = nextConfig;
