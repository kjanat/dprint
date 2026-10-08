import type { Environment } from "monaco-editor";

// Let Vite bundle worker entry points instead of using Monaco's relative URLs.
// Keep each new Worker(new URL(..., import.meta.url)) inline with a literal path:
// Vite cannot discover the entry points through helper functions or variables.
globalThis.MonacoEnvironment = {
  getWorker(_workerId, label) {
    switch (label) {
      case "typescript":
      case "javascript":
        return new Worker(new URL("monaco-editor/language/typescript/ts.worker.js", import.meta.url), { type: "module" });
      case "json":
        return new Worker(new URL("monaco-editor/language/json/json.worker.js", import.meta.url), { type: "module" });
      case "css":
      case "scss":
      case "less":
        return new Worker(new URL("monaco-editor/language/css/css.worker.js", import.meta.url), { type: "module" });
      case "html":
      case "handlebars":
      case "razor":
        return new Worker(new URL("monaco-editor/language/html/html.worker.js", import.meta.url), { type: "module" });
      default:
        return new Worker(new URL("monaco-editor/editor/editor.worker.js", import.meta.url), { type: "module" });
    }
  },
} satisfies Environment;
