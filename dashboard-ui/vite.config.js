import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  build: {
    // Embed into the server binary via rust-embed
    outDir: "../crates/rustunnel-server/src/dashboard/assets",
    emptyOutDir: true,
  },
  server: {
    // Proxy API calls to the running server during development
    proxy: {
      "/api": "http://localhost:8443",
    },
  },
});
