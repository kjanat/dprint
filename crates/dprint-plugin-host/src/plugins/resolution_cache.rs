use std::collections::BTreeMap;
use std::hash::Hash;
use std::hash::Hasher;
use std::path::PathBuf;

use dprint_plugin_types::FileMatchingInfo;
use serde::Deserialize;
use serde::Serialize;

use crate::environment::Environment;
use crate::plugins::FormatConfig;
use crate::utils::FastInsecureHasher;

/// Changes when what's stored changes, which makes older files misses.
const RESOLUTIONS_FILE_VERSION: u32 = 2;
/// How many configurations to keep per plugin (ex. for a few projects that
/// configure a plugin differently).
const MAX_RESOLUTIONS: usize = 8;

/// What a plugin resolved a configuration to.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PluginResolution {
  pub file_matching: FileMatchingInfo,
  pub resolved_config: String,
}

/// Keeps what a Wasm plugin resolved configurations to, next to its compiled
/// module in the plugin cache.
///
/// Which files a Wasm plugin formats and its resolved configuration only
/// depend on the plugin and the configuration, since a Wasm plugin can't read
/// anything else. Keeping them lets dprint know which files the plugin formats
/// without loading it, so it only loads the plugins it formats files with.
pub struct PluginResolutionCache {
  file_path: PathBuf,
  /// Identifies the compiled module the resolutions are of.
  artifact_id: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResolutionsFile {
  version: u32,
  entries: Vec<ResolutionEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResolutionEntry {
  key: u64,
  #[serde(flatten)]
  resolution: PluginResolution,
}

impl PluginResolutionCache {
  pub fn new(file_path: PathBuf, artifact_id: u64) -> Self {
    Self { file_path, artifact_id }
  }

  pub fn get(&self, environment: &impl Environment, config: &FormatConfig) -> Option<PluginResolution> {
    let key = self.key(config);
    let file = self.read(environment)?;
    file.entries.into_iter().find(|entry| entry.key == key).map(|entry| entry.resolution)
  }

  /// Keeps the resolution. This is best effort: it's only a cache, and
  /// another dprint process may write the file at the same time.
  pub fn set(&self, environment: &impl Environment, config: &FormatConfig, resolution: &PluginResolution) {
    let key = self.key(config);
    let mut entries = self.read(environment).map(|file| file.entries).unwrap_or_default();
    entries.retain(|entry| entry.key != key);
    entries.insert(
      0,
      ResolutionEntry {
        key,
        resolution: resolution.clone(),
      },
    );
    entries.truncate(MAX_RESOLUTIONS);
    let file = ResolutionsFile {
      version: RESOLUTIONS_FILE_VERSION,
      entries,
    };
    let result = serde_json::to_vec(&file)
      .map_err(anyhow::Error::from)
      .and_then(|bytes| Ok(environment.atomic_write_file_bytes(&self.file_path, &bytes)?));
    if let Err(err) = result {
      log_debug!(environment, "Failed writing {}: {:#}", self.file_path.display(), err);
    }
  }

  fn read(&self, environment: &impl Environment) -> Option<ResolutionsFile> {
    let bytes = environment.read_file_bytes(&self.file_path).ok()?;
    let file = serde_json::from_slice::<ResolutionsFile>(&bytes).ok()?;
    (file.version == RESOLUTIONS_FILE_VERSION).then_some(file)
  }

  fn key(&self, config: &FormatConfig) -> u64 {
    let mut hasher = FastInsecureHasher::default();
    hasher.write_u64(self.artifact_id);
    // As JSON, which unlike `ConfigKeyValue`'s hash keeps where arrays and
    // objects end (ex. `[[], true]` and `[[true]]` hash the same), so that
    // only the same configuration is the same key. In order, so the order of
    // the properties doesn't matter.
    let plugin_config = config.plugin.iter().collect::<BTreeMap<_, _>>();
    hasher.write(&serde_json::to_vec(&plugin_config).unwrap_or_default());
    config.global.hash(&mut hasher);
    hasher.finish()
  }
}

#[cfg(test)]
mod test {
  use dprint_configuration::ConfigKeyMap;
  use dprint_configuration::ConfigKeyValue;
  use dprint_configuration::GlobalConfiguration;
  use dprint_platform::environment::*;
  use dprint_plugin_types::FormatConfigId;
  use pretty_assertions::assert_eq;

  use super::*;
  use crate::environment::TestEnvironment;

  fn config(plugin: &[(&str, ConfigKeyValue)], line_width: Option<u32>) -> FormatConfig {
    FormatConfig {
      id: FormatConfigId::from_raw(1),
      plugin: plugin.iter().map(|(key, value)| (key.to_string(), value.clone())).collect::<ConfigKeyMap>(),
      global: GlobalConfiguration {
        line_width,
        ..Default::default()
      },
    }
  }

  #[test]
  fn keys_configurations_by_their_structure() {
    let cache = PluginResolutionCache::new(PathBuf::from("/resolutions.json"), 1);
    let array = |values: Vec<ConfigKeyValue>| ConfigKeyValue::Array(values);
    // the same values, nested differently
    let a = config(&[("value", array(vec![array(vec![]), ConfigKeyValue::Bool(true)]))], None);
    let b = config(&[("value", array(vec![array(vec![ConfigKeyValue::Bool(true)])]))], None);
    assert_ne!(cache.key(&a), cache.key(&b));
    // the order of the properties doesn't matter
    let c = config(&[("a", ConfigKeyValue::Bool(true)), ("b", ConfigKeyValue::Bool(false))], None);
    let d = config(&[("b", ConfigKeyValue::Bool(false)), ("a", ConfigKeyValue::Bool(true))], None);
    assert_eq!(cache.key(&c), cache.key(&d));
  }

  fn resolution(ext: &str) -> PluginResolution {
    PluginResolution {
      file_matching: FileMatchingInfo {
        file_extensions: vec![ext.to_string()],
        file_names: vec![],
        additive: false,
      },
      resolved_config: format!("{{\"ext\":\"{}\"}}", ext),
    }
  }

  #[test]
  fn keeps_resolutions_per_configuration() {
    let environment = TestEnvironment::new();
    let cache = PluginResolutionCache::new(PathBuf::from("/cache/plugins/a.resolutions.json"), 1);
    let a = config(&[("x", ConfigKeyValue::from_i32(1)), ("y", ConfigKeyValue::from_bool(true))], None);
    assert_eq!(cache.get(&environment, &a), None);

    cache.set(&environment, &a, &resolution("a"));
    assert_eq!(cache.get(&environment, &a), Some(resolution("a")));
    // the order of the properties doesn't matter, nor the id
    let mut reordered = config(&[("y", ConfigKeyValue::from_bool(true)), ("x", ConfigKeyValue::from_i32(1))], None);
    reordered.id = FormatConfigId::from_raw(2);
    assert_eq!(cache.get(&environment, &reordered), Some(resolution("a")));

    // a different plugin or global configuration is a different resolution
    let b = config(&[("x", ConfigKeyValue::from_i32(2)), ("y", ConfigKeyValue::from_bool(true))], None);
    let c = config(&[("x", ConfigKeyValue::from_i32(1)), ("y", ConfigKeyValue::from_bool(true))], Some(80));
    assert_eq!(cache.get(&environment, &b), None);
    assert_eq!(cache.get(&environment, &c), None);
    cache.set(&environment, &b, &resolution("b"));
    assert_eq!(cache.get(&environment, &a), Some(resolution("a")));
    assert_eq!(cache.get(&environment, &b), Some(resolution("b")));

    // and so is another build of the plugin
    let rebuilt = PluginResolutionCache::new(PathBuf::from("/cache/plugins/a.resolutions.json"), 2);
    assert_eq!(rebuilt.get(&environment, &a), None);
  }

  #[test]
  fn keeps_the_most_recent_resolutions() {
    let environment = TestEnvironment::new();
    let cache = PluginResolutionCache::new(PathBuf::from("/cache/plugins/a.resolutions.json"), 1);
    let configs = (0..MAX_RESOLUTIONS as i32 + 1)
      .map(|i| config(&[("x", ConfigKeyValue::from_i32(i))], None))
      .collect::<Vec<_>>();
    for config in &configs {
      cache.set(&environment, config, &resolution("a"));
    }
    assert_eq!(cache.get(&environment, &configs[0]), None);
    for config in &configs[1..] {
      assert_eq!(cache.get(&environment, config), Some(resolution("a")));
    }
  }

  #[test]
  fn ignores_an_invalid_file() {
    let environment = TestEnvironment::new();
    let cache = PluginResolutionCache::new(PathBuf::from("/cache/plugins/a.resolutions.json"), 1);
    let a = config(&[], None);
    environment.mk_dir_all("/cache/plugins").unwrap();
    environment.write_file("/cache/plugins/a.resolutions.json", "{").unwrap();
    assert_eq!(cache.get(&environment, &a), None);
    // and replaces it
    cache.set(&environment, &a, &resolution("a"));
    assert_eq!(cache.get(&environment, &a), Some(resolution("a")));
  }
}
