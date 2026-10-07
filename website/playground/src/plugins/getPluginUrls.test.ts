import { deepStrictEqual, rejects, strictEqual } from "node:assert/strict";
import { getPluginSchemaUrl } from "../../../src/scripts/plugin-repository.js";
import { getPluginDownloadUrl, getPluginShortNameFromPluginUrl, getPluginUrls } from "./getPluginUrls.ts";

Deno.test("downloads built-in plugins from npm without changing shared URLs", () => {
  strictEqual(
    getPluginDownloadUrl("https://plugins.dprint.dev/typescript-0.96.1.wasm"),
    "https://cdn.jsdelivr.net/npm/@dprint/typescript@0.96.1/plugin.wasm",
  );
  strictEqual(
    getPluginDownloadUrl(
      "https://plugins.dprint.dev/g-plane/markup_fmt-v0.27.5.wasm",
    ),
    "https://cdn.jsdelivr.net/npm/dprint-plugin-markup@0.27.5/plugin.wasm",
  );
  strictEqual(
    getPluginShortNameFromPluginUrl(
      "https://plugins.dprint.dev/g-plane/markup_fmt-v0.27.5.wasm",
    ),
    "markup_fmt",
  );
});

Deno.test("preserves custom plugin URLs, including plugins with matching names", () => {
  for (
    const url of [
      "https://example.com/typescript-0.96.1.wasm",
      "https://plugins.dprint.dev/other/typescript-0.96.1.wasm",
      "https://plugins.dprint.dev/custom-1.0.0.wasm",
      "https://example.com/?url=https://plugins.dprint.dev/typescript-0.96.1.wasm",
    ]
  ) {
    strictEqual(getPluginDownloadUrl(url), url);
  }
});

Deno.test("discovers all built-in plugins through CORS-enabled npm metadata", async () => {
  const originalFetch = globalThis.fetch;
  const signal = new AbortController().signal;
  const requests: string[] = [];
  globalThis.fetch = (input, init) => {
    strictEqual(init?.signal, signal);
    const url = String(input);
    strictEqual(
      url.startsWith("https://data.jsdelivr.com/v1/packages/npm/"),
      true,
    );
    requests.push(url);
    return Promise.resolve(Response.json({ version: "1.2.3" }));
  };
  try {
    const urls = await getPluginUrls(signal);
    strictEqual(requests.length, 13);
    deepStrictEqual(urls.map(getPluginShortNameFromPluginUrl), [
      "typescript",
      "json",
      "markdown",
      "toml",
      "dockerfile",
      "biome",
      "oxc",
      "mago",
      "ruff",
      "malva",
      "markup_fmt",
      "pretty_yaml",
      "pretty_graphql",
    ]);
    for (const url of urls) {
      strictEqual(
        getPluginDownloadUrl(url).startsWith("https://cdn.jsdelivr.net/npm/"),
        true,
      );
    }
  } finally {
    globalThis.fetch = originalFetch;
  }
});

Deno.test("reports metadata failures and invalid versions", async () => {
  const originalFetch = globalThis.fetch;
  try {
    globalThis.fetch = () => Promise.resolve(new Response(null, { status: 503 }));
    await rejects(
      () => getPluginUrls(new AbortController().signal),
      /HTTP 503/,
    );
    globalThis.fetch = () => Promise.resolve(Response.json({ version: null }));
    await rejects(
      () => getPluginUrls(new AbortController().signal),
      /No stable release version/,
    );
  } finally {
    globalThis.fetch = originalFetch;
  }
});

Deno.test("resolves versioned plugin schemas to the existing CORS-enabled mirror", async () => {
  strictEqual(
    await getPluginSchemaUrl(
      "https://plugins.dprint.dev/dprint/dprint-plugin-typescript/0.96.1/schema.json",
    ),
    "https://cdn.jsdelivr.net/gh/dprint/dprint-plugin-typescript@0.96.1/deployment/schema.json",
  );
  strictEqual(
    await getPluginSchemaUrl(
      "https://plugins.dprint.dev/g-plane/markup_fmt/v0.27.5/schema.json",
    ),
    "https://cdn.jsdelivr.net/gh/g-plane/markup_fmt@v0.27.5/dprint_plugin/deployment/schema.json",
  );
  strictEqual(
    await getPluginSchemaUrl("https://example.com/schema.json"),
    "https://example.com/schema.json",
  );
});
