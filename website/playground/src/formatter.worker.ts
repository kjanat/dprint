/// <reference lib="webworker" />
import { createFromBuffer, type Formatter } from "@dprint/formatter";
import { getPluginSchemaUrl } from "../../src/scripts/plugin-repository.js";
import { getPluginDownloadUrl } from "./plugins/getPluginUrls.ts";
import { formatTextUntilStable } from "./stableFormat.ts";

let formatter: Promise<Formatter> | undefined;
let config: Record<string, unknown> | undefined;
let nextFormat: { filePath: string; fileText: string } | undefined;
let abortController = new AbortController();

onmessage = (e) => {
  switch (e.data.type) {
    case "LoadUrl": {
      loadUrl(e.data.url);
      break;
    }
    case "SetConfig": {
      setConfig(e.data.config);
      break;
    }
    case "Format": {
      format(e.data.filePath, e.data.fileText);
      break;
    }
  }
};

function loadUrl(url: string) {
  abortController.abort();
  abortController = new AbortController();
  const signal = abortController.signal;
  formatter = fetch(getPluginDownloadUrl(url), { signal })
    .then((response) => {
      if (!response.ok) {
        throw new Error(`Error downloading plugin: HTTP ${response.status}`);
      }
      return response.arrayBuffer();
    })
    .then(async (wasmModuleBuffer) => {
      const newFormatter = createFromBuffer(wasmModuleBuffer);

      await postPluginInfo(newFormatter);

      setConfigSync(newFormatter, config ?? {});

      if (nextFormat) {
        formatSync(newFormatter, nextFormat.filePath, nextFormat.fileText);
      }

      return newFormatter;
    });

  formatter.catch((err: any) => {
    if (!signal.aborted) {
      postError(err);
    }
  });
}

function setConfig(providedConfig: Record<string, unknown>) {
  config = providedConfig;
  refresh();
}

function refresh() {
  if (formatter) {
    formatter.then((f) => {
      setConfigSync(f, config ?? {});
      if (nextFormat) {
        formatSync(f, nextFormat.filePath, nextFormat.fileText);
      }
    });
  }
}

function format(filePath: string, fileText: string) {
  nextFormat = { filePath, fileText };

  if (formatter) {
    formatter.then((f) => formatSync(f, filePath, fileText));
  }
}

function setConfigSync(f: Formatter, config: Record<string, unknown>) {
  doHandlingError(() => f.setConfig({}, config));
  postMessage({
    type: "FileMatching",
    info: f.getFileMatchingInfo(),
  });
}

function formatSync(f: Formatter, filePath: string, fileText: string) {
  let result;
  try {
    result = formatTextUntilStable(fileText, (fileText) => f.formatText({ filePath, fileText }));
  } catch (err: any) {
    result = err.message;
  }
  postMessage({
    type: "Format",
    text: result,
  });
}

async function postPluginInfo(f: Formatter) {
  const info = f.getPluginInfo();
  if (info.configSchemaUrl) {
    info.configSchemaUrl = await getPluginSchemaUrl(info.configSchemaUrl);
  }
  postMessage({
    type: "PluginInfo",
    info,
  });
}

function doHandlingError(action: () => void) {
  try {
    action();
  } catch (err: any) {
    postError(err);
  }
}

function postError(err: Error) {
  postMessage({
    type: "Error",
    message: err.message,
  });
}
