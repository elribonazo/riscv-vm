import type { NextConfig } from "next";

const isProd = process.env.NODE_ENV === "production";
const repoName = process.env.GITHUB_REPOSITORY?.split("/")[1] || "risk-v";

const nextConfig: NextConfig = {
  // Enable static export for GitHub Pages
  output: "export",
  
  // Set base path for GitHub Pages (repo name)
  basePath: isProd ? `/${repoName}` : "",
  
  // Asset prefix for static assets
  assetPrefix: isProd ? `/${repoName}/` : "",
  
  // Disable image optimization (not supported in static export)
  images: {
    unoptimized: true,
  },
  
  // Ensure trailing slashes for static hosting
  trailingSlash: true,
};

export default nextConfig;
