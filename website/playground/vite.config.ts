import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// https://vite.dev/config/
export default defineConfig({
  base: "/playground/",
  publicDir: "../src/assets/icons",
  plugins: [react()],
  resolve: {
    alias: {
      // react-monaco-editor still uses the path from before Monaco's export map.
      "monaco-editor/esm/vs/editor/editor.api": "monaco-editor/editor/editor.api",
    },
  },
});
