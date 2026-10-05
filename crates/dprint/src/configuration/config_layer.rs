//! Reading one configuration file: its text (a [`ConfigDocument`]) becomes
//! what it says in dprint's terms (a [`ConfigLayer`]). Its format only matters
//! for reading its values (`ConfigFileFormat::parse`), and every root property
//! dprint knows is read here, by name. Combining the layers of a configuration
//! is up to `resolve_config.rs`.

use std::borrow::Cow;

use anyhow::Result;
use anyhow::bail;
use dprint_core::configuration::ConfigKeyMap;
use dprint_core::configuration::ConfigKeyValue;
use indexmap::IndexMap;

use super::ConfigFileFormat;
use super::ConfigMap;
use super::ConfigMapValue;
use super::ConfigSettings;
use super::config_map_from_values;
use super::config_settings::filter_duplicate_plugin_sources;
use super::string_vec;
use crate::environment::Environment;
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
  /// The file it refers to, resolved against the referring file.
  pub target: PathSource,
}

impl LayerOrigin {
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
    let values = match ConfigFileFormat::from_source(source, self.file.content).parse(self.file.content) {
      Ok(values) => values,
      Err(err) => bail!("Error deserializing. {}", err),
    };
    let templates = Templates {
      file: source,
      origin: self.extended_by.last().unwrap_or(source),
    };
    let base = source.parent();

    let mut directives = ResolutionDirectives::default();
    let mut settings = ConfigSettings::default();
    let mut other_values = ConfigKeyMap::new();
    for (key, value) in values {
      match key.as_str() {
        // the configuration file's schema, for editors
        "$schema" => {}
        // an old property that's no longer used
        "projectType" => {}
        "extends" => {
          let specifiers = match value {
            ConfigKeyValue::String(specifier) => vec![specifier],
            ConfigKeyValue::Array(values) => string_vec(&key, values)?,
            _ => bail!("Extends in configuration must be a string or an array of strings."),
          };
          for specifier in templates.expand_all(specifiers)? {
            let target = resolve_url_or_file_path_to_path_source(&specifier, &base, environment)?;
            directives.extends.push(ConfigReference { target });
          }
        }
        "inherit" => directives.inherit = read_bool(&key, value)?,
        "includes" => settings.files.includes = Some(templates.expand_all(read_strings(&key, value)?)?),
        "excludes" => settings.files.excludes = templates.expand_all(read_strings(&key, value)?)?,
        "shebangs" => settings.routing.shebangs = Some(read_shebangs(value)?),
        "incremental" => settings.execution.incremental = Some(read_bool(&key, value)?),
        "plugins" => {
          let mut sources = Vec::new();
          for specifier in templates.expand_all(read_strings(&key, value)?)? {
            sources.push(parse_plugin_source_reference(&specifier, &base, environment)?);
          }
          settings.plugins.sources = filter_duplicate_plugin_sources(sources);
        }
        // the global configuration and each plugin's configuration
        _ => {
          other_values.insert(key, value);
        }
      }
    }
    settings.plugins.config = config_map_from_values(other_values)?;
    templates.expand_config_map(&mut settings.plugins.config)?;
    Ok((directives, settings))
  }
}

fn read_bool(key: &str, value: ConfigKeyValue) -> Result<bool> {
  match value {
    ConfigKeyValue::Bool(value) => Ok(value),
    _ => bail!("Expected boolean in '{}' property.", key),
  }
}

fn read_strings(key: &str, value: ConfigKeyValue) -> Result<Vec<String>> {
  match value {
    ConfigKeyValue::Array(values) => string_vec(key, values),
    _ => bail!("Expected array in '{}' property.", key),
  }
}

fn read_shebangs(value: ConfigKeyValue) -> Result<IndexMap<String, String>> {
  let ConfigKeyValue::Object(properties) = value else {
    bail!("Expected object in 'shebangs' property.");
  };
  // the shebangs and extensions are normalized here so the rest of the
  // code (ex. merging, hashing, resolution) can compare them directly
  let mut map = IndexMap::with_capacity(properties.len());
  for (mut shebang, value) in properties {
    if !shebang.starts_with("#!") || shebang.contains(['\r', '\n']) {
      bail!(
        "Expected the key '{}' in the 'shebangs' property to be a shebang line starting with '#!'.",
        shebang
      );
    }
    match value {
      ConfigKeyValue::String(extension) => {
        let extension_without_dot = extension.strip_prefix('.').unwrap_or(&extension);
        if extension_without_dot.is_empty()
          || extension_without_dot.contains(|c: char| c.is_whitespace() || matches!(c, '.' | '/' | '\\' | '*' | '?' | '[' | ']' | '{' | '}'))
        {
          bail!(
            "Expected a file extension (ex. \"sh\") for shebang '{}' in the 'shebangs' property, but found '{}'.",
            shebang,
            extension
          );
        }
        // stored lowercased and without a leading dot so it resolves the
        // same way as a real file extension
        shebang.truncate(shebang.trim_end().len());
        map.insert(shebang, extension_without_dot.to_lowercase());
      }
      _ => bail!("Expected a string file extension for shebang '{}' in the 'shebangs' property.", shebang),
    }
  }
  Ok(map)
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
  /// configuration.
  fn expand_config_map(self, config_map: &mut ConfigMap) -> Result<()> {
    for value in config_map.values_mut() {
      match value {
        ConfigMapValue::KeyValue(value) => self.expand_value(value)?,
        ConfigMapValue::PluginConfig(config) => {
          for value in config.properties.values_mut() {
            self.expand_value(value)?;
          }
        }
        ConfigMapValue::Vec(values) => {
          for value in values {
            self.expand(value)?;
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
