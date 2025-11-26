import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  // Enable standalone output for Docker
  output: "standalone",

  env: {
    NEXT_PUBLIC_RELAY_URL: process.env.NEXT_PUBLIC_RELAY_URL,
  },
  
  // Disable image optimization if needed, or keep it enabled for standalone
  images: {
    unoptimized: true,
  },
};

export default nextConfig;
