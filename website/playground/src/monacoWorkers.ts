// Deno needs explicit types for Vite's generated worker modules.
// @ts-types="./viteWorker.d.ts"
import EditorWorker from "monaco-editor/editor/editor.worker?worker";
// @ts-types="./viteWorker.d.ts"
import CssWorker from "monaco-editor/language/css/css.worker?worker";
// @ts-types="./viteWorker.d.ts"
import HtmlWorker from "monaco-editor/language/html/html.worker?worker";
// @ts-types="./viteWorker.d.ts"
import JsonWorker from "monaco-editor/language/json/json.worker?worker";
// @ts-types="./viteWorker.d.ts"
import TypeScriptWorker from "monaco-editor/language/typescript/ts.worker?worker";

// Let Vite bundle worker entry points instead of using Monaco's relative URLs.
globalThis.MonacoEnvironment = {
  getWorker(_workerId, label) {
    switch (label) {
      case "typescript":
      case "javascript":
        return new TypeScriptWorker();
      case "json":
        return new JsonWorker();
      case "css":
      case "scss":
      case "less":
        return new CssWorker();
      case "html":
      case "handlebars":
      case "razor":
        return new HtmlWorker();
      default:
        return new EditorWorker();
    }
  },
};
