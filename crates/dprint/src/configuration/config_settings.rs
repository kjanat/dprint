//! What a configuration says, in groups that each know how they combine with
//! the same group of a configuration of lower precedence: a configuration
//! file it extends (`extend`), or for a nested configuration file that
//! specifies `"inherit": true`, its ancestor's configuration (`inherit`).

use anyhow::Result;
use anyhow::bail;
use indexmap::IndexMap;

use super::ConfigMap;
use super::ConfigMapValue;
use super::remote_exec::RemoteExecProvenance;
use crate::environment::CanonicalizedPathBuf;
use crate::patterns::process_config_pattern;
use crate::plugins::PluginSourceReference;
use crate::utils::GlobPattern;
use crate::utils::GlobPatternKind;
use crate::utils::PathSource;

/// What one configuration file says about formatting.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConfigSettings {
  pub files: FileSelection,
  pub routing: FileRouting,
  pub execution: ExecutionPolicy,
  pub plugins: PluginConfiguration,
}

/// The files a configuration formats, by patterns relative to its base path.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileSelection {
  /// The files to format, or `None` for every file a plugin formats.
  pub includes: Option<Vec<String>>,
  pub excludes: Vec<String>,
}

/// How files are routed to plugins, other than by their name.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileRouting {
  /// Maps a shebang line (ex. `#!/usr/bin/env bash`) to a file extension so
  /// extensionless scripts can be routed to a plugin. `None` when the
  /// configuration doesn't say, which differs from specifying none for
  /// inheriting.
  pub shebangs: Option<IndexMap<String, String>>,
}

/// How dprint runs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExecutionPolicy {
  pub incremental: Option<bool>,
}

/// The plugins and their configuration.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PluginConfiguration {
  /// The plugins, highest precedence first.
  pub sources: Vec<PluginSourceReference>,
  /// The global configuration (ex. `lineWidth`) and each plugin's
  /// configuration by its key.
  pub config: ConfigMap,
  /// The configuration file each property of `config` is from.
  pub origins: PropertyOrigins,
  /// What remote configuration added to the exec configuration, which a
  /// nested configuration that inherits this one filters by its own
  /// `"playWithFire"` (see `remote_exec.rs`).
  pub remote_exec: RemoteExecProvenance,
}

/// The configuration file each property of a configuration is from, so a
/// diagnostic about a property can say which file to change.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PropertyOrigins {
  /// By root property: the global configuration's and each plugin's key.
  root: IndexMap<String, PathSource>,
  /// By plugin key, then by property of the plugin's configuration or its
  /// overrides.
  plugins: IndexMap<String, IndexMap<String, PathSource>>,
}

impl PropertyOrigins {
  /// Every property of a configuration file's `config`, from `source`.
  pub(super) fn of(config: &ConfigMap, source: &PathSource) -> Self {
    let mut origins = PropertyOrigins::default();
    for (key, value) in config {
      origins.root.insert(key.clone(), source.clone());
      if let ConfigMapValue::PluginConfig(plugin_config) = value {
        let properties = plugin_config
          .properties
          .keys()
          .chain(plugin_config.overrides.iter().flat_map(|override_config| override_config.properties.keys()));
        for property in properties {
          origins.add_plugin_property(key, property, source);
        }
      }
    }
    origins
  }

  /// Sets where a root property is from, unless it's already from
  /// somewhere.
  pub(super) fn add_root_property(&mut self, key: &str, source: &PathSource) {
    self.root.entry(key.to_string()).or_insert_with(|| source.clone());
  }

  /// Sets where a plugin property is from, replacing where it was from.
  pub(super) fn set_plugin_property(&mut self, plugin_key: &str, property: &str, source: &PathSource) {
    self
      .plugins
      .entry(plugin_key.to_string())
      .or_default()
      .insert(property.to_string(), source.clone());
  }

  /// Sets where a plugin property is from, unless it's already from
  /// somewhere.
  pub(super) fn add_plugin_property(&mut self, plugin_key: &str, property: &str, source: &PathSource) {
    self
      .plugins
      .entry(plugin_key.to_string())
      .or_default()
      .entry(property.to_string())
      .or_insert_with(|| source.clone());
  }

  fn add_lower_precedence(&mut self, other: PropertyOrigins) {
    for (key, source) in other.root {
      self.root.entry(key).or_insert(source);
    }
    for (plugin_key, properties) in other.plugins {
      let own = self.plugins.entry(plugin_key).or_default();
      for (property, source) in properties {
        own.entry(property).or_insert(source);
      }
    }
  }

  /// Where a root property is from, when that's not `config_source`.
  pub fn root_elsewhere(&self, property: &str, config_source: &PathSource) -> Option<&PathSource> {
    self.root.get(property).filter(|source| *source != config_source)
  }

  /// Where the properties a plugin's configuration diagnostics can be about
  /// are from (its own, then the global configuration's), when that's not
  /// `config_source`.
  pub fn plugin_elsewhere(&self, plugin_key: &str, config_source: &PathSource) -> IndexMap<String, PathSource> {
    let mut result = IndexMap::new();
    let plugin = self.plugins.get(plugin_key).into_iter().flatten();
    for (property, source) in plugin.chain(&self.root) {
      result.entry(property.clone()).or_insert_with(|| source.clone());
    }
    result.retain(|_, source| source != config_source);
    result
  }
}

impl FileSelection {
  /// Adds the excludes of an extended configuration file after this one's.
  /// An extended configuration file can't say which files to include, which
  /// is up to the configuration file that extends it.
  pub(super) fn extend(&mut self, extended: FileSelection) -> Result<()> {
    if extended.includes.is_some() {
      bail!("The 'includes' property can't be used in an extended configuration file. Specify it in the configuration file that extends it.");
    }
    self.excludes.extend(extended.excludes);
    Ok(())
  }

  /// Adds the ancestor's excludes before this one's, so this one's can opt
  /// back out of them (ex. with a `!` pattern). Each is rebased from the
  /// ancestor's directory onto this configuration's, and one that doesn't
  /// reach into it is dropped. Includes aren't inherited.
  pub(super) fn inherit(&mut self, ancestor: &FileSelection, ancestor_base: &CanonicalizedPathBuf, base: &CanonicalizedPathBuf) {
    let mut excludes = ancestor
      .excludes
      .iter()
      .filter_map(|pattern| rebase_exclude(pattern, ancestor_base, base))
      .collect::<Vec<_>>();
    excludes.append(&mut self.excludes);
    self.excludes = excludes;
  }
}

/// Rebases an exclude pattern from the directory of the configuration that
/// specifies it onto a nested directory, or `None` when it doesn't reach into
/// that directory.
pub(super) fn rebase_exclude(pattern: &str, from_base: &CanonicalizedPathBuf, to_base: &CanonicalizedPathBuf) -> Option<String> {
  // normalized the way the configuration that specifies it interprets it (ex.
  // backslash path separators, a leading `/` meaning the config's directory)
  GlobPattern::new(process_config_pattern(pattern), from_base.clone())
    .into_new_base(to_base.clone(), GlobPatternKind::Exclude)
    .map(|p| p.relative_pattern)
}

impl FileRouting {
  /// Adds the shebangs of an extended configuration file, keeping this one's
  /// extension for a shebang both specify.
  pub(super) fn extend(&mut self, extended: FileRouting) {
    if let Some(shebangs) = extended.shebangs {
      let own = self.shebangs.get_or_insert_default();
      for (shebang, extension) in shebangs {
        own.entry(shebang).or_insert(extension);
      }
    }
  }

  /// Uses the ancestor's shebangs when this configuration doesn't specify
  /// any. Specifying some replaces the ancestor's, so a nested configuration
  /// can stop routing a shebang (ex. with `"shebangs": {}`).
  pub(super) fn inherit(&mut self, ancestor: &FileRouting) {
    if self.shebangs.is_none() {
      self.shebangs.clone_from(&ancestor.shebangs);
    }
  }
}

impl ExecutionPolicy {
  pub(super) fn extend(&mut self, extended: ExecutionPolicy) {
    self.add_lower_precedence(&extended);
  }

  pub(super) fn inherit(&mut self, ancestor: &ExecutionPolicy) {
    self.add_lower_precedence(ancestor);
  }

  fn add_lower_precedence(&mut self, other: &ExecutionPolicy) {
    if self.incremental.is_none() {
      self.incremental = other.incremental;
    }
  }
}

impl PluginConfiguration {
  pub(super) fn extend(&mut self, extended: PluginConfiguration) -> Result<()> {
    self.origins.add_lower_precedence(extended.origins);
    self.add_lower_precedence(extended.sources, extended.config)
  }

  pub(super) fn inherit(&mut self, ancestor: &PluginConfiguration) -> Result<()> {
    // what remote configuration added to the ancestor's exec configuration is
    // only inherited as far as this configuration's own "playWithFire" allows
    let mut sources = ancestor.sources.clone();
    let mut config = ancestor.config.clone();
    let inherited_remote_exec = ancestor.remote_exec.filter_inherited(&self.remote_exec, &mut config, &mut sources);
    self.origins.add_lower_precedence(ancestor.origins.clone());
    self.add_lower_precedence(sources, config)?;
    self.remote_exec.inherit(inherited_remote_exec);
    self.remote_exec.remove_unused_plugin(&self.config, &mut self.sources);
    Ok(())
  }

  /// Adds plugins and configuration of lower precedence. A plugin specified
  /// in both is kept once, where it has the higher precedence. Without this a
  /// plugin listed in both would appear twice and, sharing a config key, cause
  /// the lower precedence configuration to be ignored (see issue #1043).
  fn add_lower_precedence(&mut self, sources: impl IntoIterator<Item = PluginSourceReference>, config: ConfigMap) -> Result<()> {
    self.sources.extend(sources);
    self.sources = filter_duplicate_plugin_sources(std::mem::take(&mut self.sources));
    merge_config_map_into(&mut self.config, config)
  }
}

/// Merges the lower precedence `source` config map into the higher precedence
/// `target` config map. Values already present in `target` win, while plugin
/// configurations have their properties and overrides combined.
fn merge_config_map_into(target: &mut ConfigMap, source: ConfigMap) -> Result<()> {
  for (key, value) in source {
    match value {
      ConfigMapValue::KeyValue(key_value) => {
        target.entry(key).or_insert(ConfigMapValue::KeyValue(key_value));
      }
      ConfigMapValue::Vec(items) => {
        target.entry(key).or_insert(ConfigMapValue::Vec(items));
      }
      ConfigMapValue::PluginConfig(obj) => {
        if let Some(target_obj) = target.get_mut(&key) {
          if let ConfigMapValue::PluginConfig(target_obj) = target_obj {
            // check for locked configuration
            if obj.locked && (!target_obj.properties.is_empty() || !target_obj.overrides.is_empty()) {
              bail!(
                concat!(
                  "The configuration for \"{}\" was locked, but a parent configuration specified it. ",
                  "Locked configurations cannot have their properties overridden."
                ),
                key
              );
            }

            // now the properties
            for (key, value) in obj.properties {
              target_obj.properties.entry(key).or_insert(value);
            }

            if !obj.overrides.is_empty() {
              let mut overrides = obj.overrides;
              overrides.append(&mut target_obj.overrides);
              target_obj.overrides = overrides;
            }

            // Set the associations if they aren't overwritten in the higher
            // precedence config. This is ok to do because process plugins and
            // includes/excludes aren't inherited from other config.
            if target_obj.associations.is_none() {
              target_obj.associations = obj.associations;
            }
          }
        } else {
          target.insert(key, ConfigMapValue::PluginConfig(obj));
        }
      }
    }
  }
  Ok(())
}

/// Removes plugins that specify the same source, keeping the highest precedence
/// (earliest) entry.
///
/// A discarded duplicate's checksum is kept when the entry that wins doesn't
/// specify one. Otherwise a config that specifies a plugin without a checksum
/// would discard the checksum specified for it by a lower precedence config
/// (ex. a shared config being extended), which either silently drops the
/// integrity check for a Wasm plugin or fails outright for a process plugin
/// because those require a checksum.
pub(super) fn filter_duplicate_plugin_sources(plugin_sources: Vec<PluginSourceReference>) -> Vec<PluginSourceReference> {
  let mut checksums_by_path_source: IndexMap<PathSource, Option<String>> = IndexMap::with_capacity(plugin_sources.len());

  for plugin_source in plugin_sources {
    let checksum = checksums_by_path_source.entry(plugin_source.path_source).or_default();
    if checksum.is_none() {
      *checksum = plugin_source.checksum;
    }
  }

  checksums_by_path_source
    .into_iter()
    .map(|(path_source, checksum)| PluginSourceReference { path_source, checksum })
    .collect()
}
