use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use dprint_core::configuration::ConfigKeyMap;
use dprint_core::configuration::ConfigKeyValue;
use dprint_core::plugins::ConfigChange;
use dprint_core::plugins::ConfigChangeKind;
use dprint_core::plugins::ConfigChangePathItem;
use toml_edit::DocumentMut;
use toml_edit::Item;
use toml_edit::Value;

use super::ApplyConfigChangesResult;
use super::ConfigMap;
use super::PluginUpdateInfo;
use super::add_plugins_to_config;
use super::apply_config_changes;
use super::config_map_from_values;
use super::parse_integer;
use super::parse_json_config;
use super::update_plugin_in_config;
use crate::plugins::PluginSourceReference;
use crate::utils::PathSource;
use crate::utils::parse_npm_specifier;

/// The format of a configuration file: JSON (with comments) or TOML.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFileFormat {
  Json,
  Toml,
}

impl ConfigFileFormat {
  /// The format of a configuration file at the given path, by its extension.
  pub fn from_path(path: impl AsRef<Path>) -> Self {
    match path.as_ref().extension().and_then(|ext| ext.to_str()) {
      Some(ext) if ext.eq_ignore_ascii_case("toml") => ConfigFileFormat::Toml,
      _ => ConfigFileFormat::Json,
    }
  }

  /// The format of configuration text from the given source. Text without a
  /// file extension to go by (ex. provided inline, on stdin, or from a url
  /// without one) is JSON when it starts with `{`, and TOML otherwise.
  pub fn from_source(source: &PathSource, text: &str) -> Self {
    let path = match source {
      PathSource::Local(local) if local.display.is_none() => local.path.to_string_lossy().into_owned(),
      PathSource::Local(_) => String::new(),
      PathSource::Remote(remote) => remote.url.path().to_string(),
      PathSource::Npm(npm) => npm.specifier.path.clone(),
    };
    match Path::new(&path).extension().and_then(|ext| ext.to_str()) {
      Some(ext) if ext.eq_ignore_ascii_case("toml") => ConfigFileFormat::Toml,
      Some(ext) if ext.eq_ignore_ascii_case("json") || ext.eq_ignore_ascii_case("jsonc") => ConfigFileFormat::Json,
      _ => {
        if starts_like_json(text) {
          ConfigFileFormat::Json
        } else {
          ConfigFileFormat::Toml
        }
      }
    }
  }

  /// Reads configuration text into dprint's format-neutral values. This is
  /// the only place reading a configuration file depends on its format.
  pub fn parse(self, text: &str) -> Result<ConfigKeyMap> {
    match self {
      ConfigFileFormat::Json => parse_json_config(text),
      ConfigFileFormat::Toml => parse_toml_config(text),
    }
  }

  pub fn deserialize(self, text: &str) -> Result<ConfigMap> {
    config_map_from_values(self.parse(text)?)
  }

  /// See [`add_plugins_to_config`].
  pub fn add_plugins(self, text: &str, npm_packages_to_replace: &[String], urls_to_add: &[String]) -> Result<String> {
    match self {
      ConfigFileFormat::Json => add_plugins_to_config(text, npm_packages_to_replace, urls_to_add),
      ConfigFileFormat::Toml => add_plugins_to_toml(text, npm_packages_to_replace, urls_to_add),
    }
  }

  /// See [`update_plugin_in_config`].
  pub fn update_plugin(self, text: &str, info: &PluginUpdateInfo) -> String {
    match self {
      ConfigFileFormat::Json => update_plugin_in_config(text, info),
      ConfigFileFormat::Toml => update_plugin_in_toml(text, info),
    }
  }

  /// See [`apply_config_changes`].
  pub fn apply_changes(self, text: &str, plugin_key: &str, changes: &[ConfigChange]) -> ApplyConfigChangesResult {
    match self {
      ConfigFileFormat::Json => apply_config_changes(text, plugin_key, changes),
      ConfigFileFormat::Toml => apply_toml_changes(text, plugin_key, changes),
    }
  }
}

/// Whether the text starts with `{`, ignoring whitespace and JSONC comments.
fn starts_like_json(text: &str) -> bool {
  let mut rest = text.trim_start_matches('\u{feff}');
  loop {
    rest = rest.trim_start();
    if let Some(after) = rest.strip_prefix("//") {
      rest = after.split_once('\n').map(|(_, after)| after).unwrap_or("");
    } else if let Some(after) = rest.strip_prefix("/*") {
      rest = after.split_once("*/").map(|(_, after)| after).unwrap_or("");
    } else {
      return rest.is_empty() || rest.starts_with('{');
    }
  }
}

// ---- reading ----

/// Reads TOML configuration text into the same values as the equivalent
/// JSON: tables are objects and arrays of tables are arrays of objects. TOML
/// has no null, and dates become strings.
fn parse_toml_config(text: &str) -> Result<ConfigKeyMap> {
  let document = parse_document(text)?;
  table_to_config_values(document.as_table().iter(), "")
}

fn table_to_config_values<'a>(items: impl Iterator<Item = (&'a str, &'a Item)>, path: &str) -> Result<ConfigKeyMap> {
  let mut properties = ConfigKeyMap::new();
  for (key, item) in items {
    let path = if path.is_empty() { key.to_string() } else { format!("{} -> {}", path, key) };
    if let Some(value) = item_to_config_value(item, &path)? {
      properties.insert(key.to_string(), value);
    }
  }
  Ok(properties)
}

fn item_to_config_value(item: &Item, path: &str) -> Result<Option<ConfigKeyValue>> {
  Ok(Some(match item {
    Item::None => return Ok(None),
    Item::Value(value) => toml_value_to_config_value(value, path)?,
    Item::Table(table) => ConfigKeyValue::Object(table_to_config_values(table.iter(), path)?),
    Item::ArrayOfTables(tables) => ConfigKeyValue::Array(
      tables
        .iter()
        .map(|table| table_to_config_values(table.iter(), path).map(ConfigKeyValue::Object))
        .collect::<Result<_>>()?,
    ),
  }))
}

fn toml_value_to_config_value(value: &Value, path: &str) -> Result<ConfigKeyValue> {
  Ok(match value {
    Value::String(value) => ConfigKeyValue::String(value.value().clone()),
    Value::Integer(value) => ConfigKeyValue::Number(parse_integer(&value.value().to_string(), path)?),
    // formatted with a decimal point so it's rejected like a JSON one (ex. `40.0`)
    Value::Float(value) => ConfigKeyValue::Number(parse_integer(&format!("{:?}", value.value()), path)?),
    Value::Boolean(value) => ConfigKeyValue::Bool(*value.value()),
    Value::Datetime(value) => ConfigKeyValue::String(value.value().to_string()),
    Value::Array(array) => ConfigKeyValue::Array(array.iter().map(|value| toml_value_to_config_value(value, path)).collect::<Result<_>>()?),
    Value::InlineTable(table) => {
      let mut properties = ConfigKeyMap::new();
      for (key, value) in table.iter() {
        properties.insert(key.to_string(), toml_value_to_config_value(value, &format!("{} -> {}", path, key))?);
      }
      ConfigKeyValue::Object(properties)
    }
  })
}

// ---- writing ----

/// The schema of dprint's configuration, which TOML language servers (ex.
/// tombi) pick up from a `#:schema` comment.
const SCHEMA_DIRECTIVE: &str = "#:schema https://dprint.dev/schemas/v0.json";

/// Converts a JSON configuration file to TOML. Plugin configurations become
/// tables and arrays of objects become arrays of tables.
pub fn json_config_text_to_toml(json_text: &str) -> Result<String> {
  let value = jsonc_parser::parse_to_value(json_text, &Default::default())?
    .map(jsonc_to_json)
    .unwrap_or_default();
  let serde_json::Value::Object(object) = value else {
    bail!("Expected the configuration to be an object.");
  };
  let mut document = DocumentMut::new();
  for (key, value) in object {
    if key == "$schema" {
      continue; // the schema directive replaces it
    }
    if let Some(item) = json_to_item(value, true)? {
      document.insert(&key, item);
    }
  }
  Ok(format!("{}\n\n{}", SCHEMA_DIRECTIVE, document))
}

/// Keeps the key order (serde_json's `preserve_order` is enabled).
fn jsonc_to_json(value: jsonc_parser::JsonValue) -> serde_json::Value {
  match value {
    jsonc_parser::JsonValue::Null => serde_json::Value::Null,
    jsonc_parser::JsonValue::Boolean(value) => serde_json::Value::Bool(value),
    jsonc_parser::JsonValue::Number(text) => match text.parse::<i64>() {
      Ok(value) => serde_json::Value::from(value),
      Err(_) => text
        .parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map(serde_json::Value::Number)
        .unwrap_or(serde_json::Value::Null),
    },
    jsonc_parser::JsonValue::String(text) => serde_json::Value::String(text.into_owned()),
    jsonc_parser::JsonValue::Array(values) => serde_json::Value::Array(values.into_iter().map(jsonc_to_json).collect()),
    jsonc_parser::JsonValue::Object(object) => serde_json::Value::Object(object.into_iter().map(|(key, value)| (key, jsonc_to_json(value))).collect()),
  }
}

fn json_to_item(value: serde_json::Value, is_top_level: bool) -> Result<Option<Item>> {
  Ok(Some(match value {
    serde_json::Value::Null => return Ok(None),
    serde_json::Value::Object(object) if is_top_level => {
      let mut table = toml_edit::Table::new();
      for (key, value) in object {
        if let Some(item) = json_to_item(value, false)? {
          table.insert(&key, item);
        }
      }
      Item::Table(table)
    }
    serde_json::Value::Array(values) if !values.is_empty() && values.iter().all(|value| value.is_object()) => {
      let mut tables = toml_edit::ArrayOfTables::new();
      for value in values {
        if let Some(Item::Table(table)) = json_to_item(value, true)? {
          tables.push(table);
        }
      }
      Item::ArrayOfTables(tables)
    }
    value => {
      let is_multiline_array = matches!(&value, serde_json::Value::Array(values) if values.len() > 1);
      let mut value = json_to_value(value)?;
      if is_multiline_array && let Some(array) = value.as_array_mut() {
        format_multiline(array);
      }
      Item::Value(value)
    }
  }))
}

fn json_to_value(value: serde_json::Value) -> Result<Value> {
  Ok(match value {
    serde_json::Value::Null => bail!("TOML has no null value."),
    serde_json::Value::Bool(value) => Value::from(value),
    serde_json::Value::Number(number) => match (number.as_i64(), number.as_f64()) {
      (Some(value), _) => Value::from(value),
      (None, Some(value)) => Value::from(value),
      _ => bail!("Unsupported number: {}", number),
    },
    serde_json::Value::String(value) => Value::from(value),
    serde_json::Value::Array(values) => Value::Array(values.into_iter().map(json_to_value).collect::<Result<_>>()?),
    serde_json::Value::Object(object) => {
      let mut table = toml_edit::InlineTable::new();
      for (key, value) in object {
        table.insert(&key, json_to_value(value)?);
      }
      Value::InlineTable(table)
    }
  })
}

// ---- editing ----

fn parse_document(text: &str) -> Result<DocumentMut> {
  text.parse::<DocumentMut>().map_err(|err| anyhow!("{}", err.to_string().trim_end()))
}

fn add_plugins_to_toml(text: &str, npm_packages_to_replace: &[String], urls_to_add: &[String]) -> Result<String> {
  let mut document = parse_document(text).context("Failed parsing config file.")?;
  if document.get("plugins").and_then(|item| item.as_array()).is_none() {
    if urls_to_add.is_empty() {
      return Ok(text.to_string());
    }
    document.insert("plugins", toml_edit::value(toml_edit::Array::new()));
  }
  let plugins = document.get_mut("plugins").and_then(|item| item.as_array_mut()).unwrap();
  edit_array(
    plugins,
    |value| {
      let Some(Ok(parsed)) = value.as_str().map(parse_npm_specifier) else {
        return false;
      };
      npm_packages_to_replace.contains(&parsed.specifier.name)
    },
    urls_to_add,
  );
  Ok(document.to_string())
}

/// Removes the values `should_remove` matches and appends `to_add`, keeping
/// the array's layout and comments. A comment after an element is stored
/// before the next one, so a removed element hands what's before it to the
/// element that takes its place.
fn edit_array(array: &mut toml_edit::Array, should_remove: impl Fn(&Value) -> bool, to_add: &[String]) {
  let prefix_of = |value: &Value| value.decor().prefix().and_then(|prefix| prefix.as_str()).unwrap_or("").to_string();
  let is_multiline = array.is_empty() || array.iter().any(|value| prefix_of(value).contains('\n'));
  let mut pending_prefix: Option<String> = None;
  let mut index = 0;
  while index < array.len() {
    if should_remove(array.get(index).unwrap()) {
      let removed = array.remove(index);
      pending_prefix.get_or_insert_with(|| prefix_of(&removed));
    } else {
      if let Some(prefix) = pending_prefix.take() {
        array.get_mut(index).unwrap().decor_mut().set_prefix(prefix);
      }
      index += 1;
    }
  }
  for text in to_add {
    let mut value = Value::from(text.as_str());
    let prefix = pending_prefix
      .take()
      .unwrap_or_else(|| if is_multiline { "\n  ".to_string() } else { " ".to_string() });
    value.decor_mut().set_prefix(prefix);
    value.decor_mut().set_suffix("");
    array.push_formatted(value);
  }
  if let Some(prefix) = pending_prefix {
    // the last element was removed, so what was before it goes at the end
    array.set_trailing(format!("{}\n", prefix.trim_end()));
  }
  if is_multiline {
    array.set_trailing_comma(true);
    if !array.trailing().as_str().is_some_and(|trailing| trailing.contains('\n')) {
      array.set_trailing("\n");
    }
  }
}

/// Puts each element of the array on its own line.
fn format_multiline(array: &mut toml_edit::Array) {
  for value in array.iter_mut() {
    value.decor_mut().set_prefix("\n  ");
    value.decor_mut().set_suffix("");
  }
  array.set_trailing("\n");
  array.set_trailing_comma(true);
}

fn update_plugin_in_toml(text: &str, info: &PluginUpdateInfo) -> String {
  let new_url = info.get_full_new_config_url();
  let Ok(mut document) = parse_document(text) else {
    // don't risk corrupting a file that doesn't parse
    return text.to_string();
  };
  let Some(plugins) = document.get_mut("plugins").and_then(|item| item.as_array_mut()) else {
    // the plugins likely come from an extended configuration
    return text.to_string();
  };
  let mut changed = false;
  for value in plugins.iter_mut() {
    let Some(entry) = value.as_str() else {
      continue;
    };
    if is_same_reference(entry, &info.old_reference) {
      let decor = value.decor().clone();
      *value = Value::from(new_url.as_str());
      *value.decor_mut() = decor;
      changed = true;
    }
  }
  if changed { document.to_string() } else { text.to_string() }
}

/// Whether the config entry refers to the plugin reference, which for npm
/// specifiers is compared by what they mean since they display normalized.
fn is_same_reference(entry: &str, reference: &PluginSourceReference) -> bool {
  match &reference.path_source {
    PathSource::Npm(npm_source) => {
      parse_npm_specifier(entry).is_ok_and(|parsed| parsed.specifier == npm_source.specifier && parsed.checksum == reference.checksum)
    }
    _ => entry == reference.to_string(),
  }
}

fn apply_toml_changes(text: &str, plugin_key: &str, changes: &[ConfigChange]) -> ApplyConfigChangesResult {
  let mut diagnostics = Vec::new();
  let mut document = match parse_document(text) {
    Ok(document) => document,
    Err(err) => {
      diagnostics.push(format!("Failed applying change since config file failed to parse: {:#}", err));
      return ApplyConfigChangesResult {
        new_text: text.to_string(),
        diagnostics,
      };
    }
  };

  for change in changes {
    let Some(plugin_item) = document.get_mut(plugin_key) else {
      return Default::default();
    };
    let result = match &change.kind {
      ConfigChangeKind::Add(value) => apply_toml_change(plugin_item, &change.path, TomlChange::Add(value)).map_err(|err| ("adding", err)),
      ConfigChangeKind::Set(value) => apply_toml_change(plugin_item, &change.path, TomlChange::Set(value)).map_err(|err| ("setting", err)),
      ConfigChangeKind::Remove => apply_toml_change(plugin_item, &change.path, TomlChange::Remove).map_err(|err| ("removing", err)),
    };
    if let Err((action, err)) = result {
      diagnostics.push(format!("Failed {} item at path '{}': {}", action, display_path(plugin_key, &change.path), err));
    }
  }

  ApplyConfigChangesResult {
    new_text: document.to_string(),
    diagnostics,
  }
}

enum TomlChange<'a> {
  /// Appends to an array, or adds the property.
  Add(&'a ConfigKeyValue),
  /// Replaces the existing value.
  Set(&'a ConfigKeyValue),
  Remove,
}

fn apply_toml_change(plugin_item: &mut Item, path: &[ConfigChangePathItem], change: TomlChange) -> Result<()> {
  let Some((last, parents)) = path.split_last() else {
    bail!("Expected a path.");
  };
  let mut current = TomlNode::Item(plugin_item);
  for path_item in parents {
    current = current.child(path_item)?;
  }
  match (change, last) {
    (TomlChange::Add(value), ConfigChangePathItem::String(key)) => {
      let value = config_value_to_toml(value)?;
      if let Some(array) = current.child_array(key) {
        array.push_like_siblings(value);
        Ok(())
      } else {
        current.insert(key, value)
      }
    }
    (TomlChange::Add(value), ConfigChangePathItem::Number(index)) => {
      let array = current.into_array().ok_or_else(|| anyhow!("Expected array."))?;
      if *index > array.len() {
        bail!("Expected array index '{}' to be less than the length of the array.", index);
      }
      array.insert_like_siblings(*index, config_value_to_toml(value)?);
      Ok(())
    }
    (TomlChange::Set(value), path_item) => current.child(path_item)?.set(config_value_to_toml(value)?),
    (TomlChange::Remove, ConfigChangePathItem::String(key)) => current.remove(key),
    (TomlChange::Remove, ConfigChangePathItem::Number(index)) => current.remove_index(*index),
  }
}

/// A place in a TOML document: a table entry, a value within an inline table
/// or array, or a table within an array of tables.
enum TomlNode<'a> {
  Item(&'a mut Item),
  Value(&'a mut Value),
  Table(&'a mut toml_edit::Table),
}

impl<'a> TomlNode<'a> {
  fn child(self, path_item: &ConfigChangePathItem) -> Result<TomlNode<'a>> {
    match path_item {
      ConfigChangePathItem::String(key) => match self {
        TomlNode::Item(Item::Table(table)) | TomlNode::Table(table) => table.get_mut(key).map(TomlNode::Item),
        TomlNode::Item(Item::Value(Value::InlineTable(table))) | TomlNode::Value(Value::InlineTable(table)) => table.get_mut(key).map(TomlNode::Value),
        _ => None,
      }
      .ok_or_else(|| anyhow!("Expected property '{}'.", key)),
      ConfigChangePathItem::Number(index) => match self {
        TomlNode::Item(Item::Value(Value::Array(array))) | TomlNode::Value(Value::Array(array)) => array.get_mut(*index).map(TomlNode::Value),
        TomlNode::Item(Item::ArrayOfTables(tables)) => tables.get_mut(*index).map(TomlNode::Table),
        _ => return Err(anyhow!("Expected array.")),
      }
      .ok_or_else(|| anyhow!("Expected array index '{}' to be less than the length of the array.", index)),
    }
  }

  fn child_array(&mut self, key: &str) -> Option<&mut toml_edit::Array> {
    match self {
      TomlNode::Item(Item::Table(table)) => table.get_mut(key).and_then(|item| item.as_array_mut()),
      TomlNode::Table(table) => table.get_mut(key).and_then(|item| item.as_array_mut()),
      TomlNode::Item(Item::Value(Value::InlineTable(table))) | TomlNode::Value(Value::InlineTable(table)) => {
        table.get_mut(key).and_then(|value| value.as_array_mut())
      }
      _ => None,
    }
  }

  fn into_array(self) -> Option<&'a mut toml_edit::Array> {
    match self {
      TomlNode::Item(Item::Value(Value::Array(array))) | TomlNode::Value(Value::Array(array)) => Some(array),
      _ => None,
    }
  }

  fn set(self, value: Value) -> Result<()> {
    match self {
      TomlNode::Item(item) => *item = Item::Value(value),
      TomlNode::Value(existing) => {
        let decor = existing.decor().clone();
        *existing = value;
        *existing.decor_mut() = decor;
      }
      TomlNode::Table(_) => bail!("Unsupported. Could not replace a table in an array of tables."),
    }
    Ok(())
  }

  fn insert(self, key: &str, value: Value) -> Result<()> {
    match self {
      TomlNode::Item(Item::Table(table)) | TomlNode::Table(table) => {
        table.insert(key, Item::Value(value));
      }
      TomlNode::Item(Item::Value(Value::InlineTable(table))) | TomlNode::Value(Value::InlineTable(table)) => {
        table.insert(key, value);
      }
      _ => bail!("Unsupported. Could not add into a non-table with string key '{}'", key),
    }
    Ok(())
  }

  fn remove(self, key: &str) -> Result<()> {
    let removed = match self {
      TomlNode::Item(Item::Table(table)) | TomlNode::Table(table) => table.remove(key).is_some(),
      TomlNode::Item(Item::Value(Value::InlineTable(table))) | TomlNode::Value(Value::InlineTable(table)) => table.remove(key).is_some(),
      _ => bail!("Expected object for property '{}'.", key),
    };
    if removed { Ok(()) } else { bail!("Expected property '{}'.", key) }
  }

  fn remove_index(self, index: usize) -> Result<()> {
    match self {
      TomlNode::Item(Item::Value(Value::Array(array))) | TomlNode::Value(Value::Array(array)) => {
        if index >= array.len() {
          bail!("Expected array index '{}' to be less than the length of the array.", index);
        }
        array.remove(index);
      }
      TomlNode::Item(Item::ArrayOfTables(tables)) => {
        if index >= tables.len() {
          bail!("Expected array index '{}' to be less than the length of the array.", index);
        }
        tables.remove(index);
      }
      _ => bail!("Expected array."),
    }
    Ok(())
  }
}

trait ArrayExt {
  fn push_like_siblings(&mut self, value: Value);
  fn insert_like_siblings(&mut self, index: usize, value: Value);
}

impl ArrayExt for toml_edit::Array {
  /// Adds a value, laid out like the array's existing values.
  fn push_like_siblings(&mut self, value: Value) {
    let index = self.len();
    self.insert_like_siblings(index, value);
  }

  fn insert_like_siblings(&mut self, index: usize, value: Value) {
    if self.is_empty() {
      self.push(value);
      return;
    }
    let first_prefix = layout_of(self.get(0).unwrap());
    // the whitespace between elements, ex. " " or "\n  "
    let between_prefix = match self.get(1) {
      Some(second) => layout_of(second),
      None if first_prefix.contains('\n') => first_prefix.clone(),
      None => " ".to_string(),
    };
    self.insert(index, value);
    let set_prefix = |array: &mut toml_edit::Array, index: usize, prefix: &str| {
      let decor = array.get_mut(index).unwrap().decor_mut();
      decor.set_prefix(prefix);
      decor.set_suffix("");
    };
    if index == 0 {
      // the new first element takes the old one's place
      set_prefix(self, 0, &first_prefix);
      set_prefix(self, 1, &between_prefix);
    } else {
      set_prefix(self, index, &between_prefix);
    }
  }
}

/// The whitespace before a value without any comment in it, which belongs to
/// the previous element.
fn layout_of(value: &Value) -> String {
  let prefix = value.decor().prefix().and_then(|prefix| prefix.as_str()).unwrap_or("");
  match prefix.rfind('\n') {
    Some(index) => format!("\n{}", &prefix[index + 1..]),
    None => prefix.chars().take_while(|c| c.is_whitespace()).collect(),
  }
}

fn config_value_to_toml(value: &ConfigKeyValue) -> Result<Value> {
  Ok(match value {
    ConfigKeyValue::Bool(value) => Value::from(*value),
    ConfigKeyValue::Number(value) => Value::from(*value as i64),
    ConfigKeyValue::String(value) => Value::from(value.as_str()),
    ConfigKeyValue::Array(values) => Value::Array(values.iter().map(config_value_to_toml).collect::<Result<_>>()?),
    ConfigKeyValue::Object(values) => {
      let mut table = toml_edit::InlineTable::new();
      for (key, value) in values {
        table.insert(key, config_value_to_toml(value)?);
      }
      Value::InlineTable(table)
    }
    ConfigKeyValue::Null => bail!("TOML has no null value."),
  })
}

fn display_path(plugin_key: &str, path: &[ConfigChangePathItem]) -> String {
  let mut text = plugin_key.to_string();
  for path in path {
    match path {
      ConfigChangePathItem::String(key) => {
        text.push('.');
        text.push_str(key);
      }
      ConfigChangePathItem::Number(index) => {
        text.push('[');
        text.push_str(&index.to_string());
        text.push(']');
      }
    }
  }
  text
}

#[cfg(test)]
mod test {
  use dprint_core::plugins::ConfigChange;
  use pretty_assertions::assert_eq;

  use super::*;
  use crate::environment::CanonicalizedPathBuf;

  #[test]
  fn format_from_path() {
    assert_eq!(ConfigFileFormat::from_path("dprint.toml"), ConfigFileFormat::Toml);
    assert_eq!(ConfigFileFormat::from_path("/a/.dprint.TOML"), ConfigFileFormat::Toml);
    assert_eq!(ConfigFileFormat::from_path("dprint.json"), ConfigFileFormat::Json);
    assert_eq!(ConfigFileFormat::from_path("dprint.jsonc"), ConfigFileFormat::Json);
    assert_eq!(ConfigFileFormat::from_path("config"), ConfigFileFormat::Json);
  }

  #[test]
  fn format_from_source() {
    let local = |path: &str| PathSource::new_local(CanonicalizedPathBuf::new_for_testing(path));
    assert_eq!(ConfigFileFormat::from_source(&local("/dprint.toml"), "{}"), ConfigFileFormat::Toml);
    assert_eq!(ConfigFileFormat::from_source(&local("/dprint.json"), "a = 1"), ConfigFileFormat::Json);
    let remote = |url: &str| PathSource::new_remote_from_str(url);
    assert_eq!(
      ConfigFileFormat::from_source(&remote("https://example.com/dprint.toml?ref=main"), ""),
      ConfigFileFormat::Toml
    );
    assert_eq!(
      ConfigFileFormat::from_source(&remote("https://example.com/dprint.json"), ""),
      ConfigFileFormat::Json
    );
    // without an extension (ex. inline text), the text decides
    let virtual_source = PathSource::new_local_virtual(CanonicalizedPathBuf::new_for_testing("/<config>"), "inline".to_string());
    assert_eq!(ConfigFileFormat::from_source(&virtual_source, " {}"), ConfigFileFormat::Json);
    assert_eq!(
      ConfigFileFormat::from_source(&virtual_source, "// comment\n/* other */ {}"),
      ConfigFileFormat::Json
    );
    assert_eq!(ConfigFileFormat::from_source(&virtual_source, "lineWidth = 80"), ConfigFileFormat::Toml);
    assert_eq!(
      ConfigFileFormat::from_source(&virtual_source, "# comment\n[typescript]"),
      ConfigFileFormat::Toml
    );
    assert_eq!(
      ConfigFileFormat::from_source(&remote("https://example.com/config"), "plugins = []"),
      ConfigFileFormat::Toml
    );
  }

  #[test]
  fn toml_reads_like_the_equivalent_json() {
    let toml_text = r##"#:schema https://dprint.dev/schemas/v0.json
lineWidth = 100
useTabs = true
extends = "https://example.com/base.toml"
excludes = ["**/node_modules"]
plugins = [
  "npm:@dprint/typescript@0.96.1",
  "npm:@dprint/markdown@0.25.0",
]

[shebangs]
"#!/usr/bin/env bash" = "sh"

[typescript]
quoteStyle = "preferSingle"
associations = ["**/*.ts"]

[markdown]
"codeBlock.useTabs" = false
textWrap = "maintain"

[[markdown.overrides]]
files = ["**/CHANGELOG.md"]
lineWidth = 9999

[exec]
cwd = "${configDir}"
timeout = 30

[[exec.commands]]
command = "tombi format -"
exts = ["toml"]

[[exec.commands]]
command = "rustfmt"
exts = ["rs"]
"##;
    let json_text = r##"{
  "lineWidth": 100,
  "useTabs": true,
  "extends": "https://example.com/base.toml",
  "excludes": ["**/node_modules"],
  "plugins": ["npm:@dprint/typescript@0.96.1", "npm:@dprint/markdown@0.25.0"],
  "shebangs": { "#!/usr/bin/env bash": "sh" },
  "typescript": { "quoteStyle": "preferSingle", "associations": ["**/*.ts"] },
  "markdown": {
    "codeBlock.useTabs": false,
    "textWrap": "maintain",
    "overrides": [{ "files": ["**/CHANGELOG.md"], "lineWidth": 9999 }]
  },
  "exec": {
    "cwd": "${configDir}",
    "timeout": 30,
    "commands": [
      { "command": "tombi format -", "exts": ["toml"] },
      { "command": "rustfmt", "exts": ["rs"] }
    ]
  }
}"##;
    assert_eq!(
      ConfigFileFormat::Toml.deserialize(toml_text).unwrap(),
      ConfigFileFormat::Json.deserialize(json_text).unwrap()
    );
    assert_eq!(
      ConfigFileFormat::Toml.parse(toml_text).unwrap(),
      ConfigFileFormat::Json.parse(json_text).unwrap()
    );
  }

  #[test]
  fn reads_numbers_the_same_in_both_formats() {
    fn error(format: ConfigFileFormat, text: &str) -> String {
      format.parse(text).unwrap_err().to_string()
    }
    let expected = "Expected property 'typescript -> lineWidth' with value '40.0' to be convertible to a signed integer. invalid digit found in string";
    assert_eq!(error(ConfigFileFormat::Toml, "[typescript]\nlineWidth = 40.0\n"), expected);
    assert_eq!(error(ConfigFileFormat::Json, r#"{ "typescript": { "lineWidth": 40.0 } }"#), expected);
    assert!(error(ConfigFileFormat::Toml, "lineWidth = 3000000000\n").starts_with("Expected property 'lineWidth' with value '3000000000'"));
    assert!(error(ConfigFileFormat::Json, r#"{ "lineWidth": 3000000000 }"#).starts_with("Expected property 'lineWidth' with value '3000000000'"));
  }

  #[test]
  fn toml_syntax_errors_say_where() {
    let err = ConfigFileFormat::Toml.deserialize("lineWidth = \n").unwrap_err().to_string();
    assert!(err.contains("line 1, column 13"), "{}", err);
  }

  #[test]
  fn adds_plugins_to_toml() {
    let text = r#"# my config
lineWidth = 80
plugins = [
  "npm:@dprint/json@0.21.0", # pinned
  "npm:@dprint/typescript@0.95.0",
]

[typescript]
quoteStyle = "preferSingle"
"#;
    let result = ConfigFileFormat::Toml
      .add_plugins(
        text,
        &["@dprint/typescript".to_string()],
        &["npm:@dprint/typescript@0.96.1".to_string(), "npm:@dprint/markdown@0.25.0".to_string()],
      )
      .unwrap();
    assert_eq!(
      result,
      r#"# my config
lineWidth = 80
plugins = [
  "npm:@dprint/json@0.21.0", # pinned
  "npm:@dprint/typescript@0.96.1",
  "npm:@dprint/markdown@0.25.0",
]

[typescript]
quoteStyle = "preferSingle"
"#
    );
  }

  #[test]
  fn adds_plugins_to_toml_without_a_plugins_array() {
    let result = ConfigFileFormat::Toml
      .add_plugins(
        "lineWidth = 80\n\n[typescript]\nquoteStyle = \"preferSingle\"\n",
        &[],
        &["npm:@dprint/json@0.21.0".to_string()],
      )
      .unwrap();
    // the array goes before the tables, where top level values have to be
    assert_eq!(
      result,
      "lineWidth = 80\nplugins = [\n  \"npm:@dprint/json@0.21.0\",\n]\n\n[typescript]\nquoteStyle = \"preferSingle\"\n"
    );
  }

  #[test]
  fn updates_plugins_in_toml() {
    let reference = |text: &str| {
      crate::plugins::parse_plugin_source_reference(
        text,
        &PathSource::new_local(CanonicalizedPathBuf::new_for_testing("/")),
        &crate::environment::TestEnvironment::new(),
      )
      .unwrap()
    };
    let info = PluginUpdateInfo {
      name: "dprint-plugin-json".to_string(),
      old_version: "0.21.0".to_string(),
      old_reference: reference("npm:@dprint/json@0.21.0"),
      new_version: "0.25.1".to_string(),
      new_reference: reference("npm:@dprint/json@0.25.1"),
    };
    let text = "plugins = [\n  \"npm:@dprint/json@0.21.0/plugin.wasm\", # json\n  \"npm:@dprint/markdown@0.25.0\",\n]\n";
    assert_eq!(
      ConfigFileFormat::Toml.update_plugin(text, &info),
      "plugins = [\n  \"npm:@dprint/json@0.25.1\", # json\n  \"npm:@dprint/markdown@0.25.0\",\n]\n"
    );
    // nothing to update
    let text = "plugins = [\"npm:@dprint/markdown@0.25.0\"]\n";
    assert_eq!(ConfigFileFormat::Toml.update_plugin(text, &info), text);
  }

  #[test]
  fn applies_config_changes_to_toml() {
    let text = r#"[test]
# keep me
a = 1
list = [1, 2] # numbers
nested = { inner = "value" }
removed = true
"#;
    use ConfigChangePathItem::Number as Index;
    let key = |key: &str| ConfigChangePathItem::String(key.to_string());
    let change = |path: Vec<ConfigChangePathItem>, kind: ConfigChangeKind| ConfigChange { path, kind };
    let changes = vec![
      change(vec![key("a")], ConfigChangeKind::Set(ConfigKeyValue::from_i32(5))),
      change(vec![key("list")], ConfigChangeKind::Add(ConfigKeyValue::from_i32(3))),
      change(vec![key("list"), Index(0)], ConfigChangeKind::Add(ConfigKeyValue::from_i32(0))),
      change(vec![key("nested"), key("inner")], ConfigChangeKind::Set(ConfigKeyValue::from_str("changed"))),
      change(vec![key("new")], ConfigChangeKind::Add(ConfigKeyValue::from_str("added"))),
      change(vec![key("removed")], ConfigChangeKind::Remove),
      change(vec![key("missing"), key("deep")], ConfigChangeKind::Set(ConfigKeyValue::from_i32(1))),
    ];
    let result = ConfigFileFormat::Toml.apply_changes(text, "test", &changes);
    assert_eq!(
      result.new_text,
      r#"[test]
# keep me
a = 5
list = [0, 1, 2, 3] # numbers
nested = { inner = "changed" }
new = "added"
"#
    );
    assert_eq!(
      result.diagnostics,
      vec!["Failed setting item at path 'test.missing.deep': Expected property 'missing'.".to_string()]
    );
  }

  #[test]
  fn converts_json_config_to_toml() {
    let json_text = r#"{
  "$schema": "https://dprint.dev/schemas/v0.json",
  "typescript": {},
  "markdown": { "codeBlock.useTabs": false },
  "exec": {
    "cwd": "${configDir}",
    "commands": [{ "command": "rustfmt", "exts": ["rs"] }]
  },
  "excludes": [],
  "plugins": ["npm:@dprint/typescript@0.96.1", "npm:@dprint/markdown@0.25.0"]
}"#;
    let toml_text = json_config_text_to_toml(json_text).unwrap();
    assert_eq!(
      toml_text,
      r#"#:schema https://dprint.dev/schemas/v0.json

excludes = []
plugins = [
  "npm:@dprint/typescript@0.96.1",
  "npm:@dprint/markdown@0.25.0",
]

[typescript]

[markdown]
"codeBlock.useTabs" = false

[exec]
cwd = "${configDir}"

[[exec.commands]]
command = "rustfmt"
exts = ["rs"]
"#
    );
    // and it reads back the same, apart from the schema it has as a comment
    let mut json_config = ConfigFileFormat::Json.deserialize(json_text).unwrap();
    json_config.shift_remove("$schema");
    assert_eq!(ConfigFileFormat::Toml.deserialize(&toml_text).unwrap(), json_config);
  }
}
