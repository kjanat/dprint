import { getLatestPluginVersion } from "./plugin-repository.js";

// Replaces plugin links with the latest version.

// Pre-compute quoted placeholder URLs at module load time
const pluginPlaceholders = new Map([
  [
    "\"https://plugins.dprint.dev/typescript-x.x.x.wasm\"",
    "dprint-plugin-typescript",
  ],
  ["\"https://plugins.dprint.dev/json-x.x.x.wasm\"", "dprint-plugin-json"],
  [
    "\"https://plugins.dprint.dev/markdown-x.x.x.wasm\"",
    "dprint-plugin-markdown",
  ],
  ["\"https://plugins.dprint.dev/toml-x.x.x.wasm\"", "dprint-plugin-toml"],
  [
    "\"https://plugins.dprint.dev/dockerfile-x.x.x.wasm\"",
    "dprint-plugin-dockerfile",
  ],
  ["\"https://plugins.dprint.dev/biome-x.x.x.wasm\"", "dprint-plugin-biome"],
  ["\"https://plugins.dprint.dev/oxc-x.x.x.wasm\"", "dprint-plugin-oxc"],
  ["\"https://plugins.dprint.dev/ruff-x.x.x.wasm\"", "dprint-plugin-ruff"],
  ["\"https://plugins.dprint.dev/jupyter-x.x.x.wasm\"", "dprint-plugin-jupyter"],
  ["\"https://plugins.dprint.dev/g-plane/malva-vx.x.x.wasm\"", "g-plane/malva"],
  [
    "\"https://plugins.dprint.dev/g-plane/markup_fmt-vx.x.x.wasm\"",
    "g-plane/markup_fmt",
  ],
  [
    "\"https://plugins.dprint.dev/g-plane/pretty_yaml-vx.x.x.wasm\"",
    "g-plane/pretty_yaml",
  ],
  [
    "\"https://plugins.dprint.dev/g-plane/pretty_graphql-vx.x.x.wasm\"",
    "g-plane/pretty_graphql",
  ],
  [
    "\"https://plugins.dprint.dev/jakebailey/gofumpt-vx.x.x.wasm\"",
    "jakebailey/dprint-plugin-gofumpt",
  ],
  ["\"https://plugins.dprint.dev/jolars/panache-x.x.x.wasm\"", "jolars/panache"],
  ["\"https://plugins.dprint.dev/jolars/badness-vx.x.x.wasm\"", "jolars/badness"],
  ["\"https://plugins.dprint.dev/jolars/arity-vx.x.x.wasm\"", "jolars/arity"],
  ["\"https://plugins.dprint.dev/jolars/fatou-vx.x.x.wasm\"", "jolars/fatou"],
]);

export const replacePluginUrls = () => {
  const elements = getPluginUrlElements();
  for (const element of elements) {
    const pluginName = pluginPlaceholders.get(element.textContent);
    getLatestPluginUrl(pluginName)
      .then((url) => {
        element.textContent = `\"${url}\"`;
      })
      .catch((err) => {
        console.error("Error updating plugin URLs.", err);
      });
  }
};

const getLatestPluginUrl = async (pluginName) => {
  const version = await getLatestPluginVersion(pluginName);
  const pluginPath = pluginName
    .replace(/^dprint-plugin-/, "")
    .replace("/dprint-plugin-", "/");
  const tag = pluginName.includes("/") ? "v" + version : version;
  return `https://plugins.dprint.dev/${pluginPath}-${tag}.wasm`;
};

const getPluginUrlElements = () => {
  const stringElements = document.getElementsByClassName("hljs-string");
  const result = [];
  for (let i = 0; i < stringElements.length; i++) {
    const stringElement = stringElements.item(i);
    if (pluginPlaceholders.has(stringElement.textContent)) {
      result.push(stringElement);
    }
  }
  return result;
};
