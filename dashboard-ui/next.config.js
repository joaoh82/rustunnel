/** @type {import('next').NextConfig} */
const isDev = process.env.NEXT_EXPORT === 'false';

const nextConfig = {
  // Static export for production — embedded into the Rust binary via rust-embed.
  // Disabled during local development so the dev server proxy can forward /api.
  output: isDev ? undefined : 'export',
  images: { unoptimized: true },
};

if (isDev) {
  // Proxy API calls to the running Rust server during development.
  nextConfig.rewrites = async () => [
    { source: '/api/:path*', destination: 'http://localhost:4041/api/:path*' },
  ];
}

module.exports = nextConfig;
