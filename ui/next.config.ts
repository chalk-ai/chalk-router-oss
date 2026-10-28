import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  output: "standalone",
  async redirects() {
    return [
      { source: "/providers", destination: "/routing?v=connections", permanent: false },
      { source: "/routes", destination: "/routing?v=routes", permanent: false },
      { source: "/fallback", destination: "/routing?v=routes", permanent: false },
      { source: "/judge", destination: "/routing?v=routes", permanent: false },
      { source: "/tokens", destination: "/access?v=keys", permanent: false },
      { source: "/usage-pools", destination: "/access?v=pools", permanent: false },
      { source: "/usage-budgets", destination: "/access?v=usage-budgets", permanent: false },
      { source: "/rate-limits", destination: "/access?v=rate-limits", permanent: false },
      { source: "/usage", destination: "/access?v=usage", permanent: false },
    ];
  },
};
export default nextConfig;
