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
  const repository = getPluginRepository(`${owner}/${name}`);
  const ref = version === "latest" ? await getLatestPluginVersion(repository) : version;
  // These are the tracked schemas used to create each plugin's release asset.
  let path = "schema.json";
  if (owner === "dprint") path = "deployment/schema.json";
  else if (owner === "g-plane") path = "dprint_plugin/deployment/schema.json";
  else if (repository === "jakebailey/dprint-plugin-gofumpt") path = "metadata/schema.json";
  return `https://cdn.jsdelivr.net/gh/${repository}@${encodeURIComponent(ref)}/${path}`;
};
