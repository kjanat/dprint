import { playgroundPlugins as plugins } from "../../../src/scripts/plugin-repository.ts";

export { getPluginDownloadUrl } from "../../../src/scripts/plugin-repository.ts";

export async function getPluginUrls(signal: AbortSignal): Promise<string[]> {
  return await Promise.all(
    plugins.map(async (plugin) => {
      const response = await fetch(
        `https://data.jsdelivr.com/v1/packages/npm/${plugin.npm}/resolved?specifier=latest`,
        { signal },
      );
      if (!response.ok) {
        throw new Error(
          `Error resolving ${plugin.npm}: HTTP ${response.status}`,
        );
      }
      const { version } = await response.json();
      if (typeof version !== "string" || !/^\d+\.\d+\.\d+$/.test(version)) {
        throw new Error(`No stable release version found for ${plugin.npm}.`);
      }
      // Keep canonical URLs in shared links and the plugin selector.
      return `https://plugins.dprint.dev/${plugin.prefix}${version}.wasm`;
    }),
  );
}

const RE_PLUGIN_URL = /^https:\/\/plugins\.dprint\.dev\/(?:[a-z_-]+\/)?([a-z_-]+)-v?([0-9]+\.[0-9]+\.[0-9]+)\.wasm$/;

export function getPluginShortNameFromPluginUrl(url: string) {
  const result = RE_PLUGIN_URL.exec(url);
  const name = result?.[1];
  switch (name) {
    case "typescript":
    case "markdown":
    case "json":
    case "toml":
    case "dockerfile":
    case "biome":
    case "oxc":
    case "mago":
    case "ruff":
    case "malva":
    case "markup_fmt":
    case "pretty_yaml":
    case "pretty_graphql":
      return name;
    default:
      return undefined;
  }
}

export function getLanguageFromPluginUrl(url: string) {
  const result = RE_PLUGIN_URL.exec(url);
  const language = result?.[1];
  switch (language) {
    case "typescript":
    case "markdown":
    case "json":
    case "toml":
    case "dockerfile":
      return language;
    case "biome":
      return "typescript";
    case "oxc":
      return "typescript";
    case "mago":
      return "php";
    case "ruff":
      // todo: specify python here eventually (probably need to upgrade the code editor)
      return "plaintext";
    case "malva":
      return "css";
    case "markup_fmt":
      return "html";
    case "pretty_yaml":
      return "yaml";
    default:
      return undefined;
  }
}
