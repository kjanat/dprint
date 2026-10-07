import type { Environment } from "monaco-editor";

const getWorkerUrl = (url: string, base = import.meta.url) => new URL(url, base);
const getWorker = (scriptUrl: string) => new Worker(getWorkerUrl(scriptUrl), { type: "module" });

// Let Vite bundle worker entry points instead of using Monaco's relative URLs.
globalThis.MonacoEnvironment = {
  getWorker(_workerId, label) {
    switch (label) {
      case "typescript":
      case "javascript":
        return getWorker("monaco-editor/language/typescript/ts.worker.js");
      case "json":
        return getWorker("monaco-editor/language/json/json.worker.js");
      case "css":
      case "scss":
      case "less":
        return getWorker("monaco-editor/language/css/css.worker.js");
      case "html":
      case "handlebars":
      case "razor":
        return getWorker("monaco-editor/language/html/html.worker.js");
      default:
        return getWorker("monaco-editor/editor/editor.worker.js");
    }
  },
} satisfies Environment;
