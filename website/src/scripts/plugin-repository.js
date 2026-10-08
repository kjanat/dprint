export const playgroundPlugins = [
  { name: "typescript", npm: "@dprint/typescript", prefix: "typescript-" },
  { name: "json", npm: "@dprint/json", prefix: "json-" },
  { name: "markdown", npm: "@dprint/markdown", prefix: "markdown-" },
  { name: "toml", npm: "@dprint/toml", prefix: "toml-" },
  { name: "dockerfile", npm: "@dprint/dockerfile", prefix: "dockerfile-" },
  { name: "biome", npm: "@dprint/biome", prefix: "biome-" },
  { name: "oxc", npm: "@dprint/oxc", prefix: "oxc-" },
  { name: "mago", npm: "@dprint/mago", prefix: "mago-" },
  { name: "ruff", npm: "@dprint/ruff", prefix: "ruff-" },
  { name: "malva", npm: "dprint-plugin-malva", prefix: "g-plane/malva-v" },
  { name: "markup_fmt", npm: "dprint-plugin-markup", prefix: "g-plane/markup_fmt-v" },
  { name: "pretty_yaml", npm: "dprint-plugin-yaml", prefix: "g-plane/pretty_yaml-v" },
  { name: "pretty_graphql", npm: "dprint-plugin-graphql", prefix: "g-plane/pretty_graphql-v" },
];

const downloadPlugins = [
  ...playgroundPlugins,
  { name: "pwsh", npm: "dprint-plugin-pwsh", prefix: "kjanat/pwsh-" },
];

export function getPluginDownloadUrl(url) {
  const match = /^https:\/\/plugins\.dprint\.dev\/(?:[a-zA-Z0-9_-]+\/)?([a-zA-Z0-9_-]+)-v?([0-9]+\.[0-9]+\.[0-9]+)\.wasm$/.exec(url);
  if (!match) return url;
  const plugin = downloadPlugins.find(
    (plugin) => url === `https://plugins.dprint.dev/${plugin.prefix}${match[2]}.wasm`,
  );
  // Unrecognized distributions retain their own download URL.
  return plugin
    ? `https://cdn.jsdelivr.net/npm/${plugin.npm}@${match[2]}/plugin.wasm`
    : url;
}

const latestVersions = new Map();
const pluginMirrors = new Set([
  "jolars/panache",
  "jolars/badness",
  "jolars/arity",
  "jolars/fatou",
]);

const getPluginRepository = (pluginName) => {
  if (!pluginName.includes("/")) return `dprint/${pluginName}`;
  if (pluginMirrors.has(pluginName)) return pluginName.replace("/", "/dprint-plugin-");
  return pluginName;
};

export const getLatestPluginVersion = (pluginName) => {
  const repository = getPluginRepository(pluginName);
  if (!latestVersions.has(repository)) {
    const version = fetch(`https://data.jsdelivr.com/v1/packages/gh/${repository}/resolved?specifier=latest`)
      .then(async (response) => {
        if (!response.ok) throw new Error(`Error resolving plugin version: HTTP ${response.status}`);
        const data = await response.json();
        if (typeof data.version !== "string" || data.version.length === 0) throw new Error(`No release version found for ${repository}`);
        return data.version;
      })
      .catch((err) => {
        latestVersions.delete(repository);
        throw err;
      });
    latestVersions.set(repository, version);
  }
  return latestVersions.get(repository);
};

export const getPluginSchemaUrl = async (configSchemaUrl) => {
  const url = new URL(configSchemaUrl);
  if (url.origin !== "https://plugins.dprint.dev") return configSchemaUrl;
  const [, owner, name, version, file] = url.pathname.split("/");
  if (!owner || !name || !version || file !== "schema.json") return configSchemaUrl;
  if (owner === "kjanat" && (name === "pwsh" || name === "dprint-plugin-pwsh")) {
    const repository = "kjanat/powershell-formatter";
    const ref = version === "latest" ? await getLatestPluginVersion(repository) : version;
    return `https://cdn.jsdelivr.net/gh/${repository}@${encodeURIComponent(ref)}/crates/dprint-plugin-pwsh/deployment/schema.json`;
  }
  const repository = getPluginRepository(`${owner}/${name}`);
  const ref = version === "latest" ? await getLatestPluginVersion(repository) : version;
  // These are the tracked schemas used to create each plugin's release asset.
  let path = "schema.json";
  if (owner === "dprint") path = "deployment/schema.json";
  else if (owner === "g-plane") path = "dprint_plugin/deployment/schema.json";
  else if (repository === "jakebailey/dprint-plugin-gofumpt") path = "metadata/schema.json";
  return `https://cdn.jsdelivr.net/gh/${repository}@${encodeURIComponent(ref)}/${path}`;
};
