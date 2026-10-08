use anyhow::Context;
use anyhow::Result;
use std::borrow::Borrow;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::str::Split;
use thiserror::Error;

use crate::PluginNameResolutionMaps;
use crate::configuration::ResolvedConfig;
use crate::environment::CanonicalizedPathBuf;
use crate::environment::Environment;
use crate::patterns::get_all_file_patterns;
use crate::patterns::process_cli_path_args;
use crate::utils::GlobOptions;
use crate::utils::GlobOutput;
use crate::utils::GlobPatterns;
use crate::utils::glob;
use crate::utils::read_file_shebang_line;
use dprint_discovery::ConfigDiscovery;
use dprint_discovery::FilePatternArgs;

/// Struct that allows using plugin names as a key
/// in a hash map.
#[derive(Debug, Eq, PartialEq, Hash)]
pub struct PluginNames(String);

impl PluginNames {
  const SEPARATOR: &'static str = "~~";

  pub fn names(&self) -> Split<'_, &str> {
    self.0.split(PluginNames::SEPARATOR)
  }
}

impl Borrow<str> for PluginNames {
  fn borrow(&self) -> &str {
    &self.0
  }
}

/// Builds `PluginNames` keys in a buffer it reuses, so that resolving the
/// plugins for many files doesn't allocate a key for every one of them.
#[derive(Default)]
struct PluginNamesBuilder {
  buffer: String,
}

impl PluginNamesBuilder {
  fn build(&mut self, names: &[&str]) -> PluginNamesKey<'_> {
    self.buffer.clear();
    for (i, name) in names.iter().enumerate() {
      if i > 0 {
        self.buffer.push_str(PluginNames::SEPARATOR);
      }
      self.buffer.push_str(name);
    }
    PluginNamesKey(&self.buffer)
  }
}

/// A key borrowed from a `PluginNamesBuilder`, which can be looked up without
/// allocating and only turned into a `PluginNames` when it's not in the map yet.
struct PluginNamesKey<'a>(&'a str);

impl PluginNamesKey<'_> {
  fn as_str(&self) -> &str {
    self.0
  }

  fn to_plugin_names(&self) -> PluginNames {
    PluginNames(self.0.to_string())
  }
}

#[derive(Debug, Error)]
#[error("No files found to format with the specified plugins at {}. You may want to try using `dprint output-file-paths` to see which files it's finding or run with `--allow-no-files`.", .base_path.display())]
pub struct NoFilesFoundError {
  pub base_path: CanonicalizedPathBuf,
}

pub struct FilesPathsByPlugins(HashMap<PluginNames, Vec<PathBuf>>);

impl FilesPathsByPlugins {
  pub fn ensure_not_empty(&self, base_path: &CanonicalizedPathBuf) -> Result<(), NoFilesFoundError> {
    if self.is_empty() {
      Err(NoFilesFoundError { base_path: base_path.clone() })
    } else {
      Ok(())
    }
  }

  pub fn is_empty(&self) -> bool {
    self.0.is_empty()
  }

  pub fn into_vec(self) -> Vec<(PluginNames, Vec<PathBuf>)> {
    self.0.into_iter().collect()
  }

  pub fn all_file_paths(&self) -> impl Iterator<Item = &PathBuf> {
    self.0.values().flatten()
  }

  pub fn iter(&self) -> impl Iterator<Item = (&PluginNames, &Vec<PathBuf>)> {
    self.0.iter()
  }
}

pub fn get_file_paths_by_plugins(
  plugin_name_maps: &PluginNameResolutionMaps,
  file_paths: Vec<PathBuf>,
  environment: &impl Environment,
) -> Result<FilesPathsByPlugins> {
  let mut file_paths_by_plugin: HashMap<PluginNames, Vec<PathBuf>> = HashMap::new();
  let mut plugin_names_builder = PluginNamesBuilder::default();

  for file_path in file_paths.into_iter() {
    let plugin_names = get_plugin_names_for_file_on_disk(plugin_name_maps, &file_path, environment);
    if !plugin_names.is_empty() {
      // only a handful of distinct keys exist no matter how many files there
      // are, so allocate one only when the key hasn't been seen yet
      let key = plugin_names_builder.build(&plugin_names);
      match file_paths_by_plugin.get_mut(key.as_str()) {
        Some(file_paths) => file_paths.push(file_path),
        None => {
          file_paths_by_plugin.insert(key.to_plugin_names(), vec![file_path]);
        }
      }
    }
  }

  Ok(FilesPathsByPlugins(file_paths_by_plugin))
}

/// Resolves the plugins for a file on disk, reading its shebang line when it's
/// an extensionless file that no plugin claimed by path.
pub fn get_plugin_names_for_file_on_disk<'a>(plugin_name_maps: &'a PluginNameResolutionMaps, file_path: &Path, environment: &impl Environment) -> Vec<&'a str> {
  let path_plugin_names = plugin_name_maps.get_plugin_names_from_file_path(file_path);
  if path_plugin_names.has_claiming_plugin() || !plugin_name_maps.may_match_shebang(file_path) {
    return path_plugin_names.into_names();
  }
  match read_file_shebang_line(environment, file_path) {
    // an additive plugin may still have matched by path, so keep those when the
    // shebang doesn't resolve to a plugin
    Ok(Some(shebang_line)) => plugin_name_maps
      .get_plugin_names_from_shebang(file_path, &shebang_line)
      .unwrap_or_else(|| path_plugin_names.into_names()),
    // ex. the file doesn't exist or has no shebang
    _ => path_plugin_names.into_names(),
  }
}

/// Finds files matching the config and CLI patterns.
///
/// This doesn't need the plugins, so it doesn't wait for them to load. Without
/// `includes`, it returns every file that isn't excluded.
/// `get_file_paths_by_plugins` later drops files that no plugin formats.
pub async fn get_and_resolve_file_paths(
  config: &ResolvedConfig,
  args: &FilePatternArgs,
  config_discovery: ConfigDiscovery,
  environment: &impl Environment,
) -> Result<GlobOutput> {
  let cwd = environment.cwd();
  let mut file_patterns = get_all_file_patterns(config, args, &cwd, environment);

  if args.only_staged {
    let staged_files = environment.get_staged_files().context("Failed running git staged.")?;
    file_patterns.arg_includes = Some(process_cli_path_args(&staged_files, &cwd, environment));
  } else if args.only_dirty {
    let dirty_files = environment.get_dirty_files().context("Failed running git status.")?;
    file_patterns.arg_includes = Some(process_cli_path_args(&dirty_files, &cwd, environment));
  }

  get_and_resolve_file_patterns(config, file_patterns, args.no_gitignore, config_discovery, environment).await
}

async fn get_and_resolve_file_patterns(
  config: &ResolvedConfig,
  file_patterns: GlobPatterns,
  no_gitignore: bool,
  config_discovery: ConfigDiscovery,
  environment: &impl Environment,
) -> Result<GlobOutput> {
  let cwd = environment.cwd();
  let is_cwd_in_base = cwd.starts_with(&config.origin.base_path);
  let is_in_sub_dir = cwd != config.origin.base_path && is_cwd_in_base;
  let start_dir = if is_in_sub_dir { cwd } else { config.origin.base_path.clone() };
  let environment = environment.clone();
  let pattern_base = config.origin.base_path.clone();
  let current_config_path = config.origin.source.maybe_local_path().map(|p| p.as_ref().to_path_buf());

  // This is intensive so do it in a blocking task
  dprint_async_runtime::spawn_blocking(move || {
    glob(
      &environment,
      GlobOptions {
        start_dir: start_dir.into_path_buf(),
        file_patterns,
        pattern_base,
        config_discovery,
        current_config_path,
        no_gitignore,
      },
    )
  })
  .await
  .unwrap()
}
