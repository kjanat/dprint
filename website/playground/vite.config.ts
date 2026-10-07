import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// https://vite.dev/config/
export default defineConfig({
  base: "/playground/",
  publicDir: "../src/assets/icons",
  plugins: [react()],
});
