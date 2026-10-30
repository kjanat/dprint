use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use kprint_config::plugin_config_file::CONFIG_FILE_CONTENT_PROPERTY;
use kprint_config::plugin_config_file::CONFIG_FILE_PROPERTY;
use kprint_config::plugin_config_file::ConfigFileReference;
use kprint_config::plugin_config_file::parse_config_file;
use kprint_config::plugin_config_file::select;
use kprint_configuration::ConfigKeyMap;
use kprint_configuration::ConfigKeyValue;
use kprint_plugin_types::PluginInfo;

use crate::configuration::ConfigMap;
use crate::configuration::ConfigMapValue;
use crate::configuration::RawPluginConfig;
use crate::environment::HostEnvironment;
use crate::plugins::PluginWrapper;
use kprint_config::PluginConfigFile;

pub fn get_plugin_config_map(plugin: &PluginWrapper, config_map: &mut ConfigMap) -> Result<RawPluginConfig> {
  match get_plugin_config_map_inner(plugin, config_map) {
    Ok(result) => Ok(result),
    Err(err) => bail!("Error initializing from configuration file. {:#}", err),
  }
}

fn get_plugin_config_map_inner(plugin: &PluginWrapper, config_map: &mut ConfigMap) -> Result<RawPluginConfig> {
  let config_key = &plugin.info().config_key;

  if let Some(plugin_config) = config_map.shift_remove(config_key) {
    if let ConfigMapValue::PluginConfig(plugin_config) = plugin_config {
      Ok(plugin_config)
    } else {
      bail!("Expected the configuration property '{}' to be an object.", config_key)
    }
  } else {
    Ok(Default::default())
  }
}

/// A configuration file of a plugin's own as it was when the plugin's
/// configuration was resolved, or that it didn't exist, so a long running
/// process (ex. the language server) resolves it again when it changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginConfigFileState {
  pub path: PathBuf,
  pub text: Option<String>,
}

impl PluginConfigFileState {
  pub fn is_current(&self, environment: &impl HostEnvironment) -> bool {
    let text = if environment.path_is_file(&self.path) {
      environment.read_file(&self.path).ok()
    } else {
      None
    };
    text == self.text
  }
}

/// Passes a plugin that reads a configuration file of its own (ex.
/// `tombi.toml`) that file, parsed: the one its `configFile` names, or else
/// the first of those it declares that exists. `"configFile": false` passes
/// none.
///
/// A plugin that declares no configuration files gets a `configFile` as a
/// property, so one with a `configFile` option of its own keeps it.
pub fn resolve_plugin_config_file(
  plugin_info: &PluginInfo,
  config_file: Option<PluginConfigFile>,
  mut properties: ConfigKeyMap,
  base_dir: &Path,
  environment: &impl HostEnvironment,
  files_read: &mut Vec<PluginConfigFileState>,
) -> Result<ConfigKeyMap> {
  let Some(default_files) = &plugin_info.config_files else {
    if let Some(config_file) = config_file {
      let value = match config_file {
        PluginConfigFile::Path(path) => ConfigKeyValue::String(path),
        PluginConfigFile::Disabled => ConfigKeyValue::Bool(false),
      };
      properties.insert(CONFIG_FILE_PROPERTY.to_string(), value);
    }
    return Ok(properties);
  };
  let reference = match config_file {
    Some(PluginConfigFile::Path(text)) => {
      let reference = ConfigFileReference::parse(&text).with_context(|| format!("{}.{}", plugin_info.config_key, CONFIG_FILE_PROPERTY))?;
      if !environment.path_is_file(base_dir.join(&reference.path)) {
        bail!(
          "{}.{}: Could not find {}.",
          plugin_info.config_key,
          CONFIG_FILE_PROPERTY,
          base_dir.join(&reference.path).display()
        );
      }
      Some(reference)
    }
    Some(PluginConfigFile::Disabled) => None,
    None => {
      let found = default_files.iter().find(|path| {
        let exists = environment.path_is_file(base_dir.join(path));
        if !exists {
          // one that's made later is passed instead
          files_read.push(PluginConfigFileState {
            path: base_dir.join(path),
            text: None,
          });
        }
        exists
      });
      found.map(|path| ConfigFileReference {
        path: path.clone(),
        keys: Vec::new(),
      })
    }
  };
  let Some(reference) = reference else {
    return Ok(properties);
  };
  let file_path = base_dir.join(&reference.path);
  let text = environment
    .read_file(&file_path)
    .with_context(|| format!("Failed reading {}.", file_path.display()))?;
  files_read.push(PluginConfigFileState {
    path: file_path.clone(),
    text: Some(text.clone()),
  });
  let content = parse_config_file(&file_path, &text)?;
  let content = select(&content, &reference)?;
  properties.insert(
    CONFIG_FILE_CONTENT_PROPERTY.to_string(),
    ConfigKeyValue::Object(ConfigKeyMap::from([
      ("path".to_string(), ConfigKeyValue::String(reference.path.clone())),
      ("content".to_string(), ConfigKeyValue::String(content.to_string())),
    ])),
  );
  Ok(properties)
}

#[cfg(test)]
mod tests {
  use crate::plugins::TestPlugin;
  use kprint_configuration::ConfigKeyMap;
  use kprint_configuration::ConfigKeyValue;

  use super::*;
  use crate::configuration::ConfigMap;
  use crate::configuration::ConfigMapValue;

  #[test]
  fn should_get_config_for_plugin() {
    let mut config_map = ConfigMap::new();
    let ts_plugin = RawPluginConfig {
      associations: None,
      embedded: None,
      locked: false,
      overrides: Vec::new(),
      properties: ConfigKeyMap::from([("lineWidth".to_string(), ConfigKeyValue::from_i32(40))]),
      config_file: None,
    };
    config_map.insert(String::from("lineWidth"), ConfigMapValue::from_i32(80));
    config_map.insert(String::from("typescript"), ConfigMapValue::PluginConfig(ts_plugin.clone()));
    let plugin = PluginWrapper::new(Box::new(create_plugin()));
    let result = get_plugin_config_map(&plugin, &mut config_map).unwrap();
    assert_eq!(result, ts_plugin);
    assert!(!config_map.contains_key("typescript"));
  }

  #[test]
  fn should_error_plugin_key_is_not_object() {
    let mut config_map = ConfigMap::new();
    config_map.insert(String::from("lineWidth"), ConfigMapValue::from_i32(80));
    config_map.insert(String::from("typescript"), ConfigMapValue::from(""));
    assert_errors(&mut config_map, "Expected the configuration property 'typescript' to be an object.");
  }

  fn assert_errors(config_map: &mut ConfigMap, message: &str) {
    let plugin = PluginWrapper::new(Box::new(create_plugin()));
    let result = get_plugin_config_map(&plugin, config_map);
    assert_eq!(
      result.err().unwrap().to_string(),
      format!("Error initializing from configuration file. {}", message)
    );
  }

  fn tombi_info(config_files: Option<Vec<&str>>) -> PluginInfo {
    let mut info = PluginWrapper::new(Box::new(TestPlugin::new("dprint-plugin-tombi", "toml", vec![".toml"], vec![])))
      .info()
      .clone();
    info.config_files = config_files.map(|files| files.into_iter().map(String::from).collect());
    info
  }

  fn config_file_content(properties: &ConfigKeyMap) -> Option<(String, serde_json::Value)> {
    let ConfigKeyValue::Object(object) = properties.get(CONFIG_FILE_CONTENT_PROPERTY)? else {
      panic!("not an object");
    };
    let path = object.get("path").and_then(|value| value.as_string()).unwrap().clone();
    let content = serde_json::from_str(object.get("content").and_then(|value| value.as_string()).unwrap()).unwrap();
    Some((path, content))
  }

  fn write(environment: &crate::environment::TestEnvironment, path: &str, text: &str) {
    use kprint_platform::environment::*;
    environment.mk_dir_all(Path::new(path).parent().unwrap()).unwrap();
    environment.write_file(path, text).unwrap();
  }

  fn properties(values: &[(&str, ConfigKeyValue)]) -> ConfigKeyMap {
    values.iter().map(|(key, value)| (key.to_string(), value.clone())).collect()
  }

  #[test]
  fn passes_the_first_config_file_the_plugin_declares() {
    let environment = crate::environment::TestEnvironment::new();
    write(
      &environment,
      "/project/.config/tombi.toml",
      "[format.rules]
line-width = 140
",
    );
    let info = tombi_info(Some(vec!["tombi.toml", ".config/tombi.toml"]));
    let result = resolve_plugin_config_file(&info, None, properties(&[]), Path::new("/project"), &environment, &mut Vec::new()).unwrap();
    assert_eq!(
      config_file_content(&result),
      Some((
        ".config/tombi.toml".to_string(),
        serde_json::json!({ "format": { "rules": { "line-width": 140 } } })
      ))
    );
    // none exists
    let environment = crate::environment::TestEnvironment::new();
    let result = resolve_plugin_config_file(&info, None, properties(&[]), Path::new("/project"), &environment, &mut Vec::new()).unwrap();
    assert_eq!(result, properties(&[]));
  }

  #[test]
  fn passes_the_part_of_the_config_file_it_names() {
    let environment = crate::environment::TestEnvironment::new();
    write(
      &environment,
      "/project/tombi.toml",
      "line-width = 1
",
    );
    write(
      &environment,
      "/project/pyproject.toml",
      "[tool.tombi.format.rules]
line-width = 100
",
    );
    let info = tombi_info(Some(vec!["tombi.toml"]));
    let result = resolve_plugin_config_file(
      &info,
      Some(PluginConfigFile::Path("pyproject.toml#tool.tombi".to_string())),
      properties(&[("lineWidth", ConfigKeyValue::from_i32(80))]),
      Path::new("/project"),
      &environment,
      &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(
      config_file_content(&result),
      Some((
        "pyproject.toml".to_string(),
        serde_json::json!({ "format": { "rules": { "line-width": 100 } } })
      ))
    );
    // the plugin's own properties are passed with it
    assert_eq!(result.get("lineWidth"), Some(&ConfigKeyValue::from_i32(80)));

    // false passes none
    let result = resolve_plugin_config_file(
      &info,
      Some(PluginConfigFile::Disabled),
      properties(&[]),
      Path::new("/project"),
      &environment,
      &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(result, properties(&[]));
  }

  #[test]
  fn errors_for_a_config_file_it_cant_pass() {
    let environment = crate::environment::TestEnvironment::new();
    write(&environment, "/project/tombi.toml", "a = ");
    let info = tombi_info(Some(vec!["tombi.toml"]));
    let error = |path: &str| {
      resolve_plugin_config_file(
        &info,
        Some(PluginConfigFile::Path(path.to_string())),
        properties(&[]),
        Path::new("/project"),
        &environment,
        &mut Vec::new(),
      )
      .unwrap_err()
      .to_string()
    };
    assert_eq!(
      error("missing.toml"),
      format!("toml.configFile: Could not find {}.", Path::new("/project").join("missing.toml").display())
    );
    assert!(error("tombi.toml").starts_with("Failed parsing"));
    assert!(error("tombi.toml#").starts_with("toml.configFile"));
  }

  #[test]
  fn records_the_config_files_it_looked_at_until_they_change() {
    use kprint_platform::environment::*;
    let environment = crate::environment::TestEnvironment::new();
    write(
      &environment,
      "/project/.config/tombi.toml",
      "line-width = 1
",
    );
    let info = tombi_info(Some(vec!["tombi.toml", ".config/tombi.toml", ".tombi.toml"]));
    let mut files_read = Vec::new();
    resolve_plugin_config_file(&info, None, properties(&[]), Path::new("/project"), &environment, &mut files_read).unwrap();
    // the one before it that didn't exist, and it; not the ones after it
    assert_eq!(
      files_read,
      vec![
        PluginConfigFileState {
          path: Path::new("/project").join("tombi.toml"),
          text: None,
        },
        PluginConfigFileState {
          path: Path::new("/project").join(".config/tombi.toml"),
          text: Some(
            "line-width = 1
"
            .to_string()
          ),
        },
      ]
    );
    assert!(files_read.iter().all(|file| file.is_current(&environment)));

    // changing it
    write(
      &environment,
      "/project/.config/tombi.toml",
      "line-width = 2
",
    );
    assert!(!files_read[1].is_current(&environment));
    // or making one before it
    write(&environment, "/project/tombi.toml", "");
    assert!(!files_read[0].is_current(&environment));
    // or removing it
    environment.remove_file("/project/.config/tombi.toml").unwrap();
    assert!(!files_read[1].is_current(&environment));
  }

  #[test]
  fn passes_a_plugin_without_config_files_its_config_file_as_a_property() {
    let environment = crate::environment::TestEnvironment::new();
    // ex. a plugin with a configFile option of its own
    let result = resolve_plugin_config_file(
      &tombi_info(None),
      Some(PluginConfigFile::Path("own.json".to_string())),
      properties(&[]),
      Path::new("/project"),
      &environment,
      &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(result, properties(&[(CONFIG_FILE_PROPERTY, ConfigKeyValue::from_str("own.json"))]));
    let result = resolve_plugin_config_file(&tombi_info(None), None, properties(&[]), Path::new("/project"), &environment, &mut Vec::new()).unwrap();
    assert_eq!(result, properties(&[]));
  }

  fn create_plugin() -> TestPlugin {
    TestPlugin::new("dprint-plugin-typescript", "typescript", vec![".ts"], vec![])
  }
}
