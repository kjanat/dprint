//! Reading one configuration file: its text (a [`ConfigDocument`]) becomes
//! what it says in dprint's terms (a [`ConfigLayer`]). Its format only matters
//! for parsing its values (`ConfigFileFormat::parse`); what it may hold is the
//! configuration model's (`crate::ConfigFile`, which the values
//! are read into); and what that means for resolving is converted here.
//! Combining the layers of a configuration is up to `resolve_config.rs`.

use std::borrow::Cow;

use crate::GlobalSettings;
use crate::PluginTable;
use anyhow::Result;
use anyhow::bail;
use dprint_configuration::ConfigKeyValue;
use indexmap::IndexMap;

use super::ConfigFileFormat;
use super::ConfigMap;
use super::ConfigMapValue;
use super::ConfigSettings;
use super::config_settings::filter_duplicate_plugin_sources;
use crate::environment::ConfigEnvironment as Environment;
use crate::plugins::parse_plugin_source_reference;
use crate::utils::PathSource;
use crate::utils::ResolvedFilePathWithTextRef;
use crate::utils::resolve_url_or_file_path_to_path_source;

/// A configuration file's text, before it's read.
#[derive(Debug, Clone, Copy)]
pub struct ConfigDocument<'a> {
  pub file: ResolvedFilePathWithTextRef<'a>,
  pub is_first_download: bool,
  /// The configuration files that extend this one, nearest first, so the
  /// last is the configuration file being resolved.
  pub extended_by: &'a [PathSource],
}

/// One configuration file in dprint's terms.
#[derive(Debug)]
pub struct ConfigLayer {
  pub origin: LayerOrigin,
  pub directives: ResolutionDirectives,
  pub settings: ConfigSettings,
}

/// Where a configuration file is from.
#[derive(Debug, Clone)]
pub struct LayerOrigin {
  pub source: PathSource,
  /// The configuration files that extend this one, nearest first.
  pub extended_by: Vec<PathSource>,
  /// Whether the file was downloaded for this run (ex. so a note about it is
  /// only shown once).
  pub is_first_download: bool,
}

/// What a configuration file says about how the configuration is resolved,
/// rather than about formatting. Resolving uses these up.
#[derive(Debug, Default)]
pub struct ResolutionDirectives {
  /// The configuration files it extends, highest precedence first.
  pub extends: Vec<ConfigReference>,
  /// Whether a nested (directory specific) configuration file inherits the
  /// configuration of its ancestor directory's configuration file.
  pub inherit: bool,
}

/// A configuration file another one refers to (ex. in `extends`).
#[derive(Debug, Clone)]
pub struct ConfigReference {
  /// As written, ex. `./base.json` or `https://example.com/dprint.json`.
  pub specifier: String,
  /// The file it refers to, resolved against the referring file.
  pub target: PathSource,
}

impl LayerOrigin {
  /// Whether the file is trusted the way a local file is: it's local, and so
  /// is every file that extends it. A remote file could otherwise choose a
  /// local one (ex. with `file://`) to say what it may not say itself.
  pub fn is_trusted(&self) -> bool {
    self.source.is_local() && self.extended_by.iter().all(|source| source.is_local())
  }

  /// Adds where an error is: this configuration file, then the files that
  /// extend it, other than the configuration file being resolved.
  pub fn locate(&self, err: anyhow::Error) -> anyhow::Error {
    let mut message = format!("{:#}\n    at {}", err, self.source.display());
    for source in &self.extended_by[..self.extended_by.len().saturating_sub(1)] {
      message.push_str(&format!("\n    at {}", source.display()));
    }
    anyhow::anyhow!(message)
  }
}

impl ConfigDocument<'_> {
  /// Reads the document. An error says which file it's in.
  pub fn parse(self, environment: &impl Environment) -> Result<ConfigLayer> {
    let origin = LayerOrigin {
      source: self.file.source.clone(),
      extended_by: self.extended_by.to_vec(),
      is_first_download: self.is_first_download,
    };
    match self.read(environment) {
      Ok((directives, settings)) => Ok(ConfigLayer { origin, directives, settings }),
      Err(err) => Err(origin.locate(err)),
    }
  }

  fn read(self, environment: &impl Environment) -> Result<(ResolutionDirectives, ConfigSettings)> {
    let source = self.file.source;
    let file = match ConfigFileFormat::from_source(source, self.file.content).read(self.file.content) {
      Ok(file) => file,
      Err(err) => bail!("Error deserializing. {}", err),
    };
    let templates = Templates {
      file: source,
      origin: self.extended_by.last().unwrap_or(source),
    };
    let base = source.parent();

    let mut directives = ResolutionDirectives {
      extends: Vec::new(),
      inherit: file.inherit.unwrap_or(false),
    };
    for specifier in templates.expand_all(file.extends.map(Vec::from).unwrap_or_default())? {
      let target = resolve_url_or_file_path_to_path_source(&specifier, &base, environment)?;
      directives.extends.push(ConfigReference { specifier, target });
    }

    let mut settings = ConfigSettings::default();
    settings.files.includes = file.includes.map(|includes| templates.expand_all(includes)).transpose()?;
    settings.files.excludes = templates.expand_all(file.excludes.unwrap_or_default())?;
    settings.routing.shebangs = file
      .shebangs
      .map(|shebangs| shebangs.0.into_iter().map(|(shebang, extension)| (shebang, extension.0)).collect());
    settings.execution.incremental = file.incremental;
    if let Some(plugins) = file.plugins {
      let mut sources = Vec::new();
      for specifier in templates.expand_all(plugins)? {
        sources.push(parse_plugin_source_reference(&specifier, &base, environment)?);
      }
      settings.plugins.sources = filter_duplicate_plugin_sources(sources);
    }
    settings.plugins.config = config_map(&file.global, file.plugin_tables)?;
    templates.expand_config_map(&mut settings.plugins.config)?;
    Ok((directives, settings))
  }
}

/// The global configuration and the plugins' tables of a configuration file,
/// as the map the configuration files are combined in (see
/// `config_settings.rs`).
fn config_map(global: &GlobalSettings, plugin_tables: IndexMap<String, PluginTable>) -> Result<ConfigMap> {
  let mut config = ConfigMap::new();
  for (key, value) in crate::to_values(global)? {
    config.insert(key, ConfigMapValue::KeyValue(value));
  }
  for (key, table) in plugin_tables {
    config.insert(key, ConfigMapValue::PluginConfig(table.into()));
  }
  Ok(config)
}

/// Expands the `${configDir}` and `${originConfigDir}` templates in string
/// values.
#[derive(Clone, Copy)]
struct Templates<'a> {
  /// The configuration file the values are in.
  file: &'a PathSource,
  /// The configuration file being resolved, which is the same file or one
  /// that extends it.
  origin: &'a PathSource,
}

impl Templates<'_> {
  fn expand_all(self, mut values: Vec<String>) -> Result<Vec<String>> {
    for value in &mut values {
      self.expand(value)?;
    }
    Ok(values)
  }

  /// Expands the global configuration and the properties of each plugin's
  /// configuration and its overrides.
  fn expand_config_map(self, config_map: &mut ConfigMap) -> Result<()> {
    for value in config_map.values_mut() {
      match value {
        ConfigMapValue::KeyValue(value) => self.expand_value(value)?,
        ConfigMapValue::PluginConfig(config) => {
          // an override's properties are expanded here too, as they're merged
          // with ones from other files and so can't be expanded later
          let overrides = config.overrides.iter_mut().flat_map(|override_config| override_config.properties.values_mut());
          for value in config.properties.values_mut().chain(overrides) {
            self.expand_value(value)?;
          }
        }
      }
    }
    Ok(())
  }

  fn expand_value(self, value: &mut ConfigKeyValue) -> Result<()> {
    match value {
      ConfigKeyValue::String(value) => self.expand(value)?,
      ConfigKeyValue::Array(values) => {
        for value in values {
          self.expand_value(value)?;
        }
      }
      ConfigKeyValue::Object(obj) => {
        for value in obj.values_mut() {
          self.expand_value(value)?;
        }
      }
      ConfigKeyValue::Number(_) | ConfigKeyValue::Bool(_) | ConfigKeyValue::Null => {}
    }
    Ok(())
  }

  fn expand(self, value: &mut String) -> Result<()> {
    let mut parts = Vec::with_capacity(16); // unlikely to be more than this
    let mut last_index = 0;
    let mut chars = value.char_indices().peekable();

    while let Some((index, c)) = chars.next() {
      if c == '\\' && matches!(chars.peek(), Some((_, '$'))) {
        parts.push(Cow::Borrowed(&value[last_index..index]));
        last_index = index + 1; // skip '\'
        chars.next(); // skip '$'
      } else if c == '$' && matches!(chars.peek(), Some((_, '{'))) {
        // Found start of template literal ${...}
        chars.next(); // skip '{'

        let mut template_name = "";
        let template_start_index = index + 2; // skip '{' and '$'
        for (current_index, inner_char) in chars.by_ref() {
          if inner_char == '}' {
            template_name = &value[template_start_index..current_index];
            parts.push(Cow::Borrowed(&value[last_index..index]));
            last_index = current_index + 1; // skip '}'
            break;
          }
        }

        match template_name {
          "configDir" => match self.file {
            PathSource::Local(source) => {
              parts.push(Cow::Owned(source.path.parent().unwrap().to_string_lossy().to_string()));
            }
            PathSource::Remote(_) | PathSource::Npm(_) => {
              bail!("Cannot use ${{configDir}} template in remote configuration files. Maybe use ${{originConfigDir}} instead?");
            }
          },
          "originConfigDir" => match self.origin {
            PathSource::Local(origin) => {
              parts.push(Cow::Owned(origin.path.parent().unwrap().to_string_lossy().to_string()));
            }
            PathSource::Remote(origin) => {
              bail!(
                "Cannot use ${{originConfigDir}} template when the origin configuration file ({}) is remote.",
                origin.url,
              );
            }
            PathSource::Npm(npm) => {
              bail!(
                "Cannot use ${{originConfigDir}} template when the origin configuration file ({}) is an npm package.",
                npm.specifier.display(),
              );
            }
          },
          "" => {
            // ignore
          }
          _ => {
            bail!(
              concat!(
                "Unknown template literal ${{{}}}. Only ${{configDir}} and ${{originConfigDir}} are supported. ",
                "If you meant to pass this to a plugin, escape the dollar sign with two back slashes.",
              ),
              template_name,
            );
          }
        }
      }
    }

    if !parts.is_empty() {
      parts.push(Cow::Borrowed(&value[last_index..]));
      *value = parts.join("");
    }

    Ok(())
  }
}

#[cfg(test)]
mod test {
  use dprint_configuration::ConfigKeyMap;
  use dprint_configuration::ConfigKeyValue;
  use pretty_assertions::assert_eq;

  use super::*;
  use crate::configuration::RawPluginConfig;
  use crate::configuration::RawPluginConfigOverride;

  /// The global configuration and the plugins' tables of a JSON configuration
  /// file, as they're combined.
  fn read_config_map(text: &str) -> Result<ConfigMap> {
    let file = ConfigFileFormat::Json.read(text)?;
    config_map(&file.global, file.plugin_tables)
  }

  #[test]
  fn has_the_global_configuration_as_values_and_the_tables_as_plugin_configs() {
    let mut expected = ConfigMap::new();
    expected.insert("lineWidth".to_string(), ConfigMapValue::from_i32(80));
    expected.insert("newLineKind".to_string(), ConfigMapValue::from("crlf"));
    expected.insert(
      "typescript".to_string(),
      ConfigMapValue::PluginConfig(RawPluginConfig {
        locked: false,
        associations: None,
        overrides: Vec::new(),
        properties: ConfigKeyMap::from([
          ("lineWidth".to_string(), ConfigKeyValue::from_i32(40)),
          ("preferSingleLine".to_string(), ConfigKeyValue::from_bool(true)),
          ("other".to_string(), ConfigKeyValue::from_str("test")),
          (
            "obj".to_string(),
            ConfigKeyValue::Object(ConfigKeyMap::from([("prop".to_string(), ConfigKeyValue::from_i32(5))])),
          ),
          (
            "array".to_string(),
            ConfigKeyValue::Array(vec![ConfigKeyValue::from_i32(1), ConfigKeyValue::Null]),
          ),
        ]),
      }),
    );
    assert_eq!(
      read_config_map(
        "{'lineWidth': 80, 'newLineKind': 'crlf', 'includes': [], 'typescript': { 'lineWidth': 40, 'preferSingleLine': true, 'other': 'test', 'obj': { 'prop': 5 }, 'array': [1, null] }}",
      )
      .unwrap(),
      expected
    );
    assert_eq!(read_config_map("{}").unwrap(), ConfigMap::new());
  }

  #[test]
  fn has_dprints_properties_of_a_table_apart_from_the_plugins() {
    let expected = ConfigMap::from([
      (
        "typescript".to_string(),
        ConfigMapValue::PluginConfig(RawPluginConfig {
          locked: true,
          associations: Some(vec!["test".to_string()]),
          overrides: Vec::new(),
          properties: ConfigKeyMap::from([("lineWidth".to_string(), ConfigKeyValue::from_i32(40))]),
        }),
      ),
      (
        "other".to_string(),
        ConfigMapValue::PluginConfig(RawPluginConfig {
          locked: false,
          associations: Some(vec!["other".to_string(), "test".to_string()]),
          overrides: Vec::new(),
          properties: ConfigKeyMap::new(),
        }),
      ),
    ]);
    assert_eq!(
      read_config_map(
        "{'typescript': { 'lineWidth': 40, locked: true, associations: 'test' }, 'other': { 'locked': false, 'associations': ['other', 'test'] }}"
      )
      .unwrap(),
      expected
    );
  }

  #[test]
  fn has_a_tables_overrides() {
    let expected = ConfigMap::from([(
      "typescript".to_string(),
      ConfigMapValue::PluginConfig(RawPluginConfig {
        locked: false,
        associations: None,
        overrides: vec![RawPluginConfigOverride {
          files: vec!["**/package.json".to_string(), "**/composer.json".to_string()],
          properties: ConfigKeyMap::from([
            ("indentWidth".to_string(), ConfigKeyValue::from_i32(4)),
            ("useTabs".to_string(), ConfigKeyValue::from_bool(false)),
          ]),
          origin: Default::default(),
        }],
        properties: ConfigKeyMap::from([("lineWidth".to_string(), ConfigKeyValue::from_i32(80))]),
      }),
    )]);
    assert_eq!(
      read_config_map(
        "{'typescript': { 'lineWidth': 80, 'overrides': { 'files': ['**/package.json', '**/composer.json'], 'indentWidth': 4, 'useTabs': false } }}",
      )
      .unwrap(),
      expected
    );

    let expected = ConfigMap::from([(
      "typescript".to_string(),
      ConfigMapValue::PluginConfig(RawPluginConfig {
        locked: false,
        associations: None,
        overrides: vec![
          RawPluginConfigOverride {
            files: vec!["**/package.json".to_string()],
            properties: ConfigKeyMap::from([("indentWidth".to_string(), ConfigKeyValue::from_i32(4))]),
            origin: Default::default(),
          },
          RawPluginConfigOverride {
            files: vec!["**/special-package.json".to_string()],
            properties: ConfigKeyMap::from([("lineWidth".to_string(), ConfigKeyValue::from_i32(80))]),
            origin: Default::default(),
          },
        ],
        properties: ConfigKeyMap::new(),
      }),
    )]);
    assert_eq!(
      read_config_map(
        "{'typescript': { 'overrides': [{ 'files': '**/package.json', 'indentWidth': 4 }, { 'files': ['**/special-package.json'], 'lineWidth': 80 }] }}",
      )
      .unwrap(),
      expected
    );
  }

  #[test]
  fn errors_are_the_models() {
    // what a configuration file may hold is the model's to say (see its
    // tests); a few of its errors, as they come out here
    let error = |text: &str| read_config_map(text).unwrap_err().to_string();
    assert_eq!(
      error("{'prop': null}"),
      "prop: invalid type: null, expected a plugin's configuration (an object), as a property that isn't one of dprint's"
    );
    assert_eq!(
      error("{'typescript': { 'associations': [1] }}"),
      "typescript.associations: The 'associations' property in a plugin configuration must be a string or an array of strings."
    );
    assert_eq!(
      error("{'typescript': { locked: 1 }}"),
      "typescript.locked: invalid type: integer `1`, expected a boolean"
    );
    assert_eq!(
      error("{'typescript': { 'overrides': [{ 'indentWidth': 4 }] }}"),
      "typescript.overrides[0]: missing field `files`"
    );
    assert_eq!(
      error("{'typescript': { 'overrides': [{ 'files': [], 'indentWidth': 4 }] }}"),
      "typescript.overrides[0].files: A plugin configuration override must specify at least one file pattern."
    );
    assert_eq!(
      error("{'typescript': { 'overrides': [{ 'files': '**/package.json' }] }}"),
      "typescript.overrides[0]: A plugin configuration override must specify at least one configuration property."
    );
  }
}
