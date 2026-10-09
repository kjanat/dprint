import type { Environment } from "monaco-editor";
import createEditorWorker from "monaco-editor/editor/editor.worker.js?worker";
import createCssWorker from "monaco-editor/language/css/css.worker.js?worker";
import createHtmlWorker from "monaco-editor/language/html/html.worker.js?worker";
import createJsonWorker from "monaco-editor/language/json/json.worker.js?worker";
import createTypeScriptWorker from "monaco-editor/language/typescript/ts.worker.js?worker";

globalThis.MonacoEnvironment = {
  getWorker(_workerId, label) {
    switch (label) {
      case "typescript":
      case "javascript":
        return createTypeScriptWorker();
      case "json":
        return createJsonWorker();
      case "css":
      case "scss":
      case "less":
        return createCssWorker();
      case "html":
      case "handlebars":
      case "razor":
        return createHtmlWorker();
      default:
        return createEditorWorker();
    }
  },
} satisfies Environment;
