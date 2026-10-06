//! One schema for a configuration file: dprint's, with each plugin's
//! schema for its table.

use std::collections::HashSet;
use std::collections::VecDeque;

use anyhow::Result;
use anyhow::bail;
use serde_json::Map;
use serde_json::Value;
use url::Url;

use crate::pointer;
use crate::translate::Dialect;
use crate::translate::ResourceIndex;
use crate::translate::UnknownDialect;
use crate::translate::to_2020_12;

/// The schema of a plugin's configuration.
pub struct PluginSchema {
  pub config_key: String,
  pub schema: Value,
  /// Where the schema is from, which its relative references are relative
  /// to (unless it has an `$id` saying otherwise). `None` for a schema built
  /// into dprint.
  pub url: Option<Url>,
}

/// The schema of a configuration file.
pub struct ConfigSchema {
  pub schema: Value,
  /// What about the plugins' schemas the user should know.
  pub warnings: Vec<String>,
}

/// Where a schema built into dprint, which has no url, is taken to be from.
const BUILT_IN_URI_PREFIX: &str = "dprint-built-in:/";

/// Builds one schema for a configuration file: dprint's schema (see
/// [`crate::root_schema`]), with each plugin's schema for its table. It
/// isn't necessarily self-contained, as what a plugin's schema refers to in
/// other documents stays a url.
///
/// Each plugin's schema is a schema resource of its own under `$defs`, as
/// JSON schema 2020-12 bundles them: it keeps its `$id` (or gets its url as
/// one) and its references, which are resolved against that, and is
/// translated to 2020-12 where its draft said the same thing differently
/// (see [`to_2020_12`]). A plugin's table is what dprint says every plugin
/// table is (an object, with its own properties such as `associations`),
/// which also has to be valid by the plugin's schema, whatever that says
/// (ex. `true`). The plugin's schema gets dprint's properties too, because
/// some tools check each part of an `allOf` separately when they report
/// properties a schema doesn't have (ex. tombi's strict mode), which would
/// otherwise flag a plugin's `associations`.
///
/// What the plugin's schema describes is the plugin's own properties: dprint
/// hands the plugin its table without `associations`, `locked` and
/// `overrides`, and each override without `files`. Its properties mean the
/// same of the table as of that (see [`TableSchemas`]), and each override
/// gets a schema of them too (see [`override_schema`]). A plugin schema that
/// describes its table as a whole (ex. with `maxProperties`) can't describe
/// that, so it isn't applied, with a warning.
///
/// A plugin schema of a draft dprint doesn't know is referred to by its url.
pub fn build_config_schema(plugins: Vec<PluginSchema>) -> Result<ConfigSchema> {
  let Value::Object(mut root) = crate::root_schema() else {
    bail!("Expected dprint's configuration schema to be an object.");
  };
  // a file of its own, not the one the website serves
  root.shift_remove("$id");
  // what every plugin table may have (ex. `associations`), which a plugin's
  // own schema doesn't describe, and `files` of every override
  let plugin_table_properties = root
    .get("$defs")
    .and_then(|definitions| definitions.get("pluginTable"))
    .and_then(|table| table.get("properties"))
    .and_then(Value::as_object)
    .cloned()
    .unwrap_or_default();
  let files = root
    .get("$defs")
    .and_then(|definitions| definitions.get("pluginOverride"))
    .and_then(|schema| schema.get("properties"))
    .and_then(|properties| properties.get("files"))
    .cloned()
    .unwrap_or(Value::Bool(true));

  let mut warnings = Vec::new();
  for plugin in plugins {
    let definition_name = format!("plugin:{}", plugin.config_key);
    let plugin_display = format!(
      "{} plugin{}",
      plugin.config_key,
      plugin.url.as_ref().map(|url| format!(" ({})", url)).unwrap_or_default()
    );
    let description = format!("The configuration of the {} plugin.", plugin.config_key);
    // dprint's properties of this plugin's table, with its overrides checked
    // against the plugin's properties
    let mut table_properties = plugin_table_properties.clone();
    let resource = match Resource::of(plugin.schema, plugin.url.as_ref(), &plugin.config_key) {
      Ok(resource) => resource,
      Err(UnknownDialect { dialect, pointer }) => {
        let at = if pointer.is_empty() { String::new() } else { format!(" (at {})", pointer) };
        match &plugin.url {
          Some(url) => {
            warnings.push(format!(
              concat!(
                "The configuration schema of the {} is for {}{}, a JSON schema draft dprint doesn't know, so it's referred to by its url instead. ",
                "Editors may report dprint's own properties of its table (ex. `associations`) as unknown."
              ),
              plugin_display, dialect, at
            ));
            object_entry(&mut root, "properties").insert(
              plugin.config_key,
              serde_json::json!({ "description": description, "type": "object", "properties": table_properties, "allOf": [{ "$ref": url.as_str() }] }),
            );
          }
          None => warnings.push(format!(
            "The configuration schema of the {} plugin is for {}{}, a JSON schema draft dprint doesn't know, so it's left out.",
            plugin.config_key, dialect, at
          )),
        }
        continue;
      }
    };
    let table_schemas = TableSchemas::of(&resource);
    if let Some((pointer, keyword)) = table_schemas.whole_object {
      warnings.push(format!(
        concat!(
          "The configuration schema of the {} describes its table as a whole (`{}`{}), which can't say what it does of the table dprint ",
          "hands the plugin (without `associations`, `locked` and `overrides`), so it isn't applied and editors don't check the plugin's options."
        ),
        plugin_display,
        keyword,
        if pointer.is_empty() { String::new() } else { format!(" at {}", pointer) }
      ));
      object_entry(&mut root, "properties").insert(
        plugin.config_key,
        serde_json::json!({ "description": description, "type": "object", "properties": table_properties }),
      );
      continue;
    }
    if let Some(reference) = table_schemas.not_followed {
      warnings.push(format!(
        "The configuration schema of the {} refers to {} for its table, so editors may report dprint's own properties of its table (ex. `associations`) as unknown.",
        plugin_display, reference
      ));
    }
    // the schema of its overrides goes in the plugin's resource, as what a
    // resource refers to has to be within it (or at an absolute uri)
    let override_name = override_definition_name(&resource.schema);
    let override_reference = pointer::resource_reference(resource.uri.as_str(), &pointer::append("/$defs", &override_name));
    let override_schema = override_schema(&resource, &table_schemas.unconditional, &table_properties, &files);
    if let Some((pointer, keyword)) = &table_schemas.conditional_properties {
      warnings.push(format!(
        concat!(
          "The configuration schema of the {} says what properties its table may have on a condition too (`{}` at {}), which an override ",
          "can't be checked on by itself, as the table it's merged into decides it, so editors check an override's properties only as far ",
          "as the schema says unconditionally."
        ),
        plugin_display, keyword, pointer
      ));
    }
    if let Some(overrides) = table_properties.get_mut("overrides") {
      *overrides = override_table_property(overrides, &override_reference);
    }
    let Resource { mut schema, uri, index } = resource;
    add_table_properties(&mut schema, &index, &table_properties);
    if let Value::Object(schema) = &mut schema {
      object_entry(schema, "$defs").insert(override_name, override_schema);
    }
    object_entry(&mut root, "$defs").insert(definition_name, schema);
    // dprint's table, which the plugin's schema applies to as well
    object_entry(&mut root, "properties").insert(
      plugin.config_key,
      serde_json::json!({
        "description": description,
        "type": "object",
        "properties": table_properties,
        "allOf": [{ "$ref": uri.as_str() }],
      }),
    );
  }

  let mut result = Map::new();
  // put the comment near the top
  if let Some(schema) = root.shift_remove("$schema") {
    result.insert("$schema".to_string(), schema);
  }
  result.insert(
    "$comment".to_string(),
    Value::String(format!(
      "Generated by `dprint schema` for the configuration's plugins. `dprint add` and `dprint config update` regenerate a {} next to the configuration file.",
      crate::CONFIG_SCHEMA_FILE_NAME
    )),
  );
  result.extend(root);
  Ok(ConfigSchema {
    schema: Value::Object(result),
    warnings,
  })
}

/// The name for the schema of a plugin's overrides in its resource's
/// `$defs`, which the resource doesn't use for anything else.
fn override_definition_name(resource: &Value) -> String {
  const NAME: &str = "dprint-override";
  let taken = |name: &str| resource.get("$defs").is_some_and(|definitions| definitions.get(name).is_some());
  if !taken(NAME) {
    return NAME.to_string();
  }
  (2..).map(|n| format!("{}-{}", NAME, n)).find(|name| !taken(name)).unwrap()
}

fn object_entry<'a>(object: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
  let value = object.entry(key.to_string()).or_insert_with(|| Value::Object(Map::new()));
  if !value.is_object() {
    *value = Value::Object(Map::new());
  }
  value.as_object_mut().unwrap()
}

/// A plugin's schema as a 2020-12 schema resource.
struct Resource {
  schema: Value,
  /// Its `$id`.
  uri: Url,
  index: ResourceIndex,
}

impl Resource {
  fn of(schema: Value, url: Option<&Url>, config_key: &str) -> Result<Self, UnknownDialect> {
    let dialect = match schema.get("$schema") {
      None => Dialect::DEFAULT,
      Some(Value::String(uri)) => Dialect::of(uri).ok_or_else(|| UnknownDialect {
        dialect: uri.clone(),
        pointer: String::new(),
      })?,
      Some(other) => {
        return Err(UnknownDialect {
          dialect: other.to_string(),
          pointer: String::new(),
        });
      }
    };
    // a schema built into dprint has nothing relative references could be to
    let mut from = url
      .cloned()
      .unwrap_or_else(|| Url::parse(&format!("{}{}/schema.json", BUILT_IN_URI_PREFIX, config_key)).expect("a built-in schema's uri parses"));
    from.set_fragment(None);
    // the resource's uri: its `$id`, resolved against where it's from
    let id_keyword = if dialect == Dialect::Draft04 { "id" } else { "$id" };
    let mut uri = match schema.get(id_keyword) {
      Some(Value::String(id)) => from.join(id).unwrap_or(from),
      _ => from,
    };
    uri.set_fragment(None);
    let mut schema = match schema {
      // a plugin whose schema is `false` takes no configuration of its own,
      // which leaves dprint's properties of its table
      Value::Bool(false) => serde_json::json!({ "additionalProperties": false }),
      Value::Bool(true) => serde_json::json!({}),
      schema => schema,
    };
    to_2020_12(&mut schema, dialect, &uri)?;
    let index = ResourceIndex::of(&schema, &uri);
    Ok(Self { schema, uri, index })
  }
}

/// The schemas in a plugin's schema that apply to its table: the plugin's
/// schema, what a `$ref` of one points to, and what its `allOf`, `anyOf`,
/// `oneOf`, `if`, `then`, `else`, `not` and `dependentSchemas` apply to the
/// table.
///
/// Their keywords about properties (`properties`, `additionalProperties`,
/// `required`, `dependentRequired`) mean the same of the table as of the
/// plugin's own properties of it, once dprint's properties are listed with
/// the plugin's (see [`add_table_properties`]). A keyword about the object
/// as a whole doesn't: `minProperties`, `maxProperties`, `propertyNames`,
/// `const`, `enum`, and `patternProperties` (a pattern may cover dprint's
/// property names), so a schema with one can't describe the table.
struct TableSchemas {
  /// Where the first keyword about the table as a whole is, and which.
  whole_object: Option<(String, &'static str)>,
  /// The schemas that apply to the table unconditionally (the plugin's
  /// schema, its `$ref` targets and `allOf` items), whose properties every
  /// override may set.
  unconditional: Vec<String>,
  /// Where the first schema that applies to the table on a condition (ex.
  /// an `anyOf` branch) says what properties it may have, and the keyword
  /// it's under. An override can't be checked against it on its own, as
  /// the table it's merged into decides the condition.
  conditional_properties: Option<(String, &'static str)>,
  /// A reference to another document that couldn't be followed.
  not_followed: Option<String>,
}

impl TableSchemas {
  fn of(resource: &Resource) -> Self {
    const WHOLE_OBJECT_KEYWORDS: &[&str] = &["minProperties", "maxProperties", "propertyNames", "const", "enum", "patternProperties"];
    let mut result = Self {
      whole_object: None,
      unconditional: Vec::new(),
      conditional_properties: None,
      not_followed: None,
    };
    // each schema to visit: where it is, and the keyword it applies to the
    // table on a condition of, if it doesn't apply unconditionally
    let mut pending = VecDeque::from([(String::new(), None)]);
    let mut visited = HashSet::new();
    // nearest first, so a warning names the first of what's at a level
    while let Some((pointer, condition)) = pending.pop_front() {
      if !visited.insert(pointer.clone()) {
        continue;
      }
      let Some(Value::Object(object)) = resource.schema.pointer(&pointer) else {
        continue;
      };
      // (what's next to a `$ref` applies too, in 2020-12)
      if let Some(reference) = object.get("$ref") {
        match reference.as_str().and_then(|reference| resource.index.resolve(reference, &pointer)) {
          Some(target) => pending.push_back((target, condition)),
          None => {
            result
              .not_followed
              .get_or_insert_with(|| reference.as_str().map(ToOwned::to_owned).unwrap_or_else(|| reference.to_string()));
          }
        }
      }
      if result.whole_object.is_none()
        && let Some(keyword) = WHOLE_OBJECT_KEYWORDS.iter().find(|keyword| object.contains_key(**keyword))
      {
        result.whole_object = Some((pointer.clone(), keyword));
      }
      match condition {
        None => result.unconditional.push(pointer.clone()),
        Some(keyword) => {
          let constrains_properties = object.contains_key("properties")
            || ["additionalProperties", "unevaluatedProperties"]
              .iter()
              .any(|keyword| object.get(*keyword).is_some_and(|allowed| allowed != &Value::Bool(true)));
          if constrains_properties && result.conditional_properties.is_none() {
            result.conditional_properties = Some((pointer.clone(), keyword));
          }
        }
      }
      for (keyword, condition) in [("allOf", condition), ("anyOf", Some("anyOf")), ("oneOf", Some("oneOf"))] {
        if let Some(Value::Array(schemas)) = object.get(keyword) {
          pending.extend((0..schemas.len()).map(|index| (format!("{}/{}/{}", pointer, keyword, index), condition)));
        }
      }
      for keyword in ["if", "then", "else", "not"] {
        if object.contains_key(keyword) {
          pending.push_back((format!("{}/{}", pointer, keyword), Some(keyword)));
        }
      }
      if let Some(Value::Object(dependencies)) = object.get("dependentSchemas") {
        for name in dependencies.keys() {
          pending.push_back((pointer::append(&format!("{}/dependentSchemas", pointer), name), Some("dependentSchemas")));
        }
      }
    }
    // the plugin's schema first
    result.unconditional.sort();
    result
  }
}

/// The schema of an override of a plugin's table, which is `files` and the
/// plugin's own properties (what dprint hands the plugin for the files, so
/// nothing is required): each property as the plugin's schema has it in the
/// schemas that apply to the table unconditionally (`unconditional`, see
/// [`TableSchemas`]), referred to where it is in the plugin's resource,
/// other properties as those allow. What a schema says on a condition (ex. an
/// `anyOf` branch) isn't in it, as the table an override is merged into
/// decides the condition, which is warned about (see
/// [`TableSchemas::conditional_properties`]).
fn override_schema(resource: &Resource, unconditional: &[String], table_properties: &Map<String, Value>, files: &Value) -> Value {
  let mut properties = Map::new();
  let mut additional = Vec::new();
  let mut closed = false;
  for pointer in unconditional {
    let Some(Value::Object(object)) = resource.schema.pointer(pointer) else {
      continue;
    };
    if let Some(Value::Object(declared)) = object.get("properties") {
      for name in declared.keys().filter(|name| !table_properties.contains_key(*name)) {
        let reference = serde_json::json!({ "$ref": resource.index.reference_to(&pointer::append(&format!("{}/properties", pointer), name)) });
        match properties.get_mut(name) {
          None => {
            properties.insert(name.clone(), reference);
          }
          // declared more than once: all of them apply
          Some(Value::Object(existing)) => match existing.get_mut("allOf") {
            Some(Value::Array(all)) => all.push(reference),
            _ => {
              let first = Value::Object(std::mem::take(existing));
              existing.insert("allOf".to_string(), Value::Array(vec![first, reference]));
            }
          },
          Some(_) => {}
        }
      }
    }
    for keyword in ["additionalProperties", "unevaluatedProperties"] {
      match object.get(keyword) {
        Some(Value::Bool(false)) => closed = true,
        Some(Value::Object(_)) => additional.push(serde_json::json!({ "$ref": resource.index.reference_to(&format!("{}/{}", pointer, keyword)) })),
        _ => {}
      }
    }
  }
  // `files`, as dprint describes it for every override
  let mut all_properties = Map::new();
  all_properties.insert("files".to_string(), files.clone());
  all_properties.extend(properties);
  let mut result = Map::new();
  result.insert("type".to_string(), Value::String("object".to_string()));
  result.insert("required".to_string(), serde_json::json!(["files"]));
  result.insert("minProperties".to_string(), serde_json::json!(2));
  result.insert("properties".to_string(), Value::Object(all_properties));
  if closed {
    result.insert("additionalProperties".to_string(), Value::Bool(false));
  } else if additional.len() == 1 {
    result.insert("additionalProperties".to_string(), additional.pop().unwrap());
  } else if !additional.is_empty() {
    result.insert("additionalProperties".to_string(), serde_json::json!({ "allOf": additional }));
  }
  Value::Object(result)
}

/// dprint's `overrides` property of a plugin's table, with each override
/// checked by the schema at `reference` rather than the one for any plugin.
fn override_table_property(generic: &Value, reference: &str) -> Value {
  let mut property = Map::new();
  if let Some(description) = generic.get("description") {
    property.insert("description".to_string(), description.clone());
  }
  property.insert(
    "anyOf".to_string(),
    serde_json::json!([{ "$ref": reference }, { "type": "array", "items": { "$ref": reference } }]),
  );
  Value::Object(property)
}

/// Adds what every plugin table may have (`properties`) to the schemas in a
/// plugin's resource that say what properties its table may have.
///
/// Those are the plugin's schema, or what its `$ref` points to, and the
/// schemas its `allOf`, `anyOf`, `oneOf`, `then` and `else` apply to the
/// table that list properties (ex. with `additionalProperties: false`).
fn add_table_properties(schema: &mut Value, index: &ResourceIndex, properties: &Map<String, Value>) {
  let mut pending = vec![(String::new(), true)];
  let mut visited = HashSet::new();
  let mut targets = Vec::new();
  while let Some((pointer, is_table_schema)) = pending.pop() {
    if !visited.insert(pointer.clone()) {
      continue;
    }
    let Some(Value::Object(object)) = schema.pointer(&pointer) else {
      continue;
    };
    let reference = object.get("$ref").and_then(Value::as_str);
    if let Some(target) = reference.and_then(|reference| index.resolve(reference, &pointer)) {
      pending.push((target, is_table_schema));
    }
    // a `$ref` says where the table's schema is, unless what's next to it
    // (which applies too, in 2020-12) says what properties the table has
    if (is_table_schema && reference.is_none())
      || ["properties", "patternProperties", "additionalProperties", "unevaluatedProperties"]
        .iter()
        .any(|key| object.contains_key(*key))
    {
      targets.push(pointer.clone());
    }
    for keyword in ["allOf", "anyOf", "oneOf"] {
      if let Some(Value::Array(schemas)) = object.get(keyword) {
        pending.extend((0..schemas.len()).map(|index| (format!("{}/{}/{}", pointer, keyword, index), false)));
      }
    }
    for keyword in ["then", "else"] {
      if object.contains_key(keyword) {
        pending.push((format!("{}/{}", pointer, keyword), false));
      }
    }
  }
  for pointer in targets {
    if let Some(Value::Object(object)) = schema.pointer_mut(&pointer) {
      let table_properties = object_entry(object, "properties");
      for (key, value) in properties {
        table_properties.entry(key.clone()).or_insert_with(|| value.clone());
      }
    }
  }
}

#[cfg(test)]
mod test {
  use pretty_assertions::assert_eq;
  use serde_json::json;

  use super::*;

  const URL: &str = "https://plugins.dprint.dev/test/schema.json";

  /// Validates `instance` with the configuration schema, as a validator
  /// that reads the whole document in its root's dialect (2020-12) does.
  fn validate_with_schema(schema: &Value, instance: &Value) -> Result<(), String> {
    const URL: &str = "https://dprint.dev/test/dprint.schema.json";
    let mut schemas = boon::Schemas::new();
    let mut compiler = boon::Compiler::new();
    compiler.add_resource(URL, schema.clone()).unwrap();
    let index = compiler.compile(URL, &mut schemas).map_err(|err| format!("Invalid schema: {:#}", err)).unwrap();
    schemas.validate(instance, index).map_err(|err| format!("{:#}", err))
  }

  /// The configuration schema with the plugin's schema, downloaded from `url`,
  /// as `test`.
  fn build(schema: Value, url: Option<&str>) -> ConfigSchema {
    build_config_schema(vec![PluginSchema {
      config_key: "test".to_string(),
      schema,
      url: url.map(|url| Url::parse(url).unwrap()),
    }])
    .unwrap()
  }

  /// What dprint says a plugin's table is, with the plugin's schema at
  /// `reference` applying to it as well. An embedded plugin's overrides are
  /// checked by its own override schema.
  fn table_schema(key: &str, reference: &str, embedded: bool) -> Value {
    let base = crate::root_schema();
    let mut properties = base["$defs"]["pluginTable"]["properties"].clone();
    if embedded {
      let override_reference = format!("{}#/$defs/dprint-override", reference);
      properties["overrides"] = json!({
        "description": properties["overrides"]["description"],
        "anyOf": [{ "$ref": override_reference }, { "type": "array", "items": { "$ref": override_reference } }],
      });
    }
    json!({
      "description": format!("The configuration of the {} plugin.", key),
      "type": "object",
      "properties": properties,
      "allOf": [{ "$ref": reference }],
    })
  }

  #[test]
  fn embeds_plugin_schemas_as_resources() {
    let schema = build_config_schema(vec![PluginSchema {
      config_key: "typescript".to_string(),
      schema: json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "$id": "https://plugins.dprint.dev/typescript/schema.json",
        "type": "object",
        "definitions": { "quoteStyle": { "type": "string", "enum": ["alwaysSingle", "preferSingle"] } },
        "properties": {
          "quoteStyle": { "$ref": "#/definitions/quoteStyle" },
          "absolute": { "$ref": "https://plugins.dprint.dev/typescript/schema.json#/definitions/quoteStyle" },
        },
      }),
      url: Some(Url::parse("https://plugins.dprint.dev/typescript/schema.json").unwrap()),
    }])
    .unwrap();
    assert_eq!(schema.warnings, Vec::<String>::new());
    let schema = schema.schema;

    let root = schema.as_object().unwrap();
    assert_eq!(root.keys().take(2).collect::<Vec<_>>(), vec!["$schema", "$comment"]);
    assert_eq!(schema["$schema"], json!("https://json-schema.org/draft/2020-12/schema"));
    assert!(root.get("$id").is_none());
    // dprint's own properties are there
    assert!(schema["properties"]["lineWidth"].is_object());
    assert!(schema["properties"]["plugins"].is_object());
    assert_eq!(
      schema["properties"]["typescript"],
      table_schema("typescript", "https://plugins.dprint.dev/typescript/schema.json", true)
    );

    // the plugin's schema is a resource of its own, with its references as they are
    let plugin = &schema["$defs"]["plugin:typescript"];
    assert_eq!(plugin["$id"], json!("https://plugins.dprint.dev/typescript/schema.json"));
    assert!(plugin.get("$schema").is_none());
    assert_eq!(plugin["properties"]["quoteStyle"], json!({ "$ref": "#/definitions/quoteStyle" }));
    assert_eq!(
      plugin["properties"]["absolute"],
      json!({ "$ref": "https://plugins.dprint.dev/typescript/schema.json#/definitions/quoteStyle" })
    );
    // dprint's properties of every plugin table were added
    for key in ["associations", "locked"] {
      assert_eq!(plugin["properties"][key], schema["$defs"]["pluginTable"]["properties"][key], "{}", key);
    }
    // with its overrides checked by the plugin's own properties
    assert_eq!(
      plugin["properties"]["overrides"],
      table_schema("typescript", "https://plugins.dprint.dev/typescript/schema.json", true)["properties"]["overrides"]
    );
    assert_eq!(
      plugin["$defs"]["dprint-override"],
      json!({
        "type": "object",
        "required": ["files"],
        "minProperties": 2,
        "properties": {
          "files": schema["$defs"]["pluginOverride"]["properties"]["files"],
          "quoteStyle": { "$ref": "https://plugins.dprint.dev/typescript/schema.json#/properties/quoteStyle" },
          "absolute": { "$ref": "https://plugins.dprint.dev/typescript/schema.json#/properties/absolute" },
        },
      })
    );
    // dprint's own definitions are still there for those
    assert!(schema["$defs"]["pluginOverride"].is_object());

    // which a validator resolves as the plugin's own document
    let validate = |table: Value| validate_with_schema(&schema, &json!({ "typescript": table }));
    assert_eq!(
      validate(
        json!({ "quoteStyle": "alwaysSingle", "absolute": "preferSingle", "locked": true, "overrides": [{ "files": "*.ts", "quoteStyle": "preferSingle" }] })
      ),
      Ok(())
    );
    assert!(validate(json!({ "quoteStyle": "double" })).is_err());
    assert!(validate(json!({ "absolute": "double" })).is_err());
    assert!(validate(json!({ "overrides": [{ "files": "*.ts", "quoteStyle": "double" }] })).is_err());
    assert!(validate(json!({ "locked": "yes" })).is_err());
  }

  #[test]
  fn escapes_plugin_config_keys_in_pointers() {
    let schema = build_config_schema(vec![PluginSchema {
      config_key: "a/b~c d".to_string(),
      schema: json!({ "properties": { "x": { "$ref": "#" } } }),
      url: None,
    }])
    .unwrap()
    .schema;
    assert_eq!(
      schema["properties"]["a/b~c d"],
      table_schema("a/b~c d", "dprint-built-in:/a/b~c%20d/schema.json", true)
    );
    assert_eq!(schema["$defs"]["plugin:a/b~c d"]["$id"], json!("dprint-built-in:/a/b~c%20d/schema.json"));
    assert_eq!(schema["$defs"]["plugin:a/b~c d"]["properties"]["x"], json!({ "$ref": "#" }));
    assert!(schema["$defs"]["plugin:a/b~c d"]["$defs"]["dprint-override"].is_object());
  }

  #[test]
  fn gives_a_resource_the_uri_its_from_unless_it_names_itself() {
    let schema = build(json!({ "type": "object" }), Some(URL));
    assert_eq!(schema.schema["$defs"]["plugin:test"]["$id"], json!(URL));
    assert_eq!(schema.schema["properties"]["test"]["allOf"], json!([{ "$ref": URL }]));

    // an `$id` of its own, relative to where it's from
    let schema = build(json!({ "$id": "v1/schema.json", "type": "object" }), Some(URL));
    assert_eq!(
      schema.schema["$defs"]["plugin:test"]["$id"],
      json!("https://plugins.dprint.dev/test/v1/schema.json")
    );
    assert_eq!(
      schema.schema["properties"]["test"]["allOf"],
      json!([{ "$ref": "https://plugins.dprint.dev/test/v1/schema.json" }])
    );
    let schema = build(json!({ "$id": "https://example.com/schema.json#", "type": "object" }), Some(URL));
    assert_eq!(schema.schema["$defs"]["plugin:test"]["$id"], json!("https://example.com/schema.json"));

    // a schema built into dprint has nothing relative references could be to
    let schema = build(json!({ "properties": { "shared": { "$ref": "shared.json" } } }), None);
    assert_eq!(schema.schema["$defs"]["plugin:test"]["$id"], json!("dprint-built-in:/test/schema.json"));
    assert_eq!(schema.schema["$defs"]["plugin:test"]["properties"]["shared"], json!({ "$ref": "shared.json" }));
  }

  #[test]
  fn keeps_references_to_named_and_nested_schemas() {
    // a draft-07 anchor is an `$id` that's a fragment
    let schema = build(
      json!({
        "definitions": { "color": { "$id": "#kleur", "type": "string", "enum": ["red", "green"] } },
        "properties": {
          "color": { "$ref": "#kleur" },
          "absolute": { "$ref": "https://plugins.dprint.dev/test/schema.json#kleur" },
        },
      }),
      Some(URL),
    );
    assert_eq!(schema.warnings, Vec::<String>::new());
    let plugin = &schema.schema["$defs"]["plugin:test"];
    assert_eq!(plugin["definitions"]["color"]["$anchor"], json!("kleur"));
    assert!(plugin["definitions"]["color"].get("$id").is_none());
    assert_eq!(plugin["properties"]["color"], json!({ "$ref": "#kleur" }));
    let validate = |table: Value| validate_with_schema(&schema.schema, &json!({ "test": table }));
    assert_eq!(validate(json!({ "color": "red", "absolute": "green" })), Ok(()));
    assert!(validate(json!({ "color": "blue" })).is_err());
    assert!(validate(json!({ "absolute": "blue" })).is_err());

    // a nested resource keeps its own uri, which its references are relative to
    let schema = build(
      json!({
        "$id": "https://example.com/test.json",
        "definitions": {
          "inner": {
            "$id": "inner/schema.json",
            "type": "object",
            "definitions": { "y": { "type": "number" } },
            "properties": {
              "y": { "$ref": "#/definitions/y" },
              "outer": { "$ref": "../test.json#/definitions/z" },
            },
          },
          "z": { "type": "string" },
        },
        "properties": {
          "inner": { "$ref": "inner/schema.json" },
          "y": { "$ref": "https://example.com/inner/schema.json#/definitions/y" },
          "z": { "$ref": "#/definitions/z" },
        },
      }),
      Some(URL),
    );
    assert_eq!(schema.warnings, Vec::<String>::new());
    let plugin = &schema.schema["$defs"]["plugin:test"];
    assert_eq!(plugin["$id"], json!("https://example.com/test.json"));
    assert_eq!(plugin["definitions"]["inner"]["$id"], json!("https://example.com/inner/schema.json"));
    assert_eq!(plugin["definitions"]["inner"]["properties"]["y"], json!({ "$ref": "#/definitions/y" }));
    assert_eq!(
      schema.schema["$defs"]["plugin:test"]["$defs"]["dprint-override"]["properties"]["inner"],
      json!({ "$ref": "https://example.com/test.json#/properties/inner" })
    );
    let validate = |table: Value| validate_with_schema(&schema.schema, &json!({ "test": table }));
    assert_eq!(validate(json!({ "inner": { "y": 1, "outer": "z" }, "y": 2, "z": "z" })), Ok(()));
    assert!(validate(json!({ "inner": { "y": "one" } })).is_err());
    assert!(validate(json!({ "inner": { "outer": 1 } })).is_err());
    assert!(validate(json!({ "y": "two" })).is_err());
    assert!(validate(json!({ "overrides": [{ "files": "*.x", "inner": { "y": "one" } }] })).is_err());
  }

  #[test]
  fn allows_dprints_properties_of_a_table_whose_schema_is_a_reference() {
    let config = json!({ "test": { "a": "a", "associations": "**/*.a", "locked": true } });
    // draft-07 ignores what's next to a `$ref`
    let schema = build(
      json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "$ref": "#/definitions/config",
        "definitions": {
          "config": { "$ref": "#/definitions/closed" },
          "closed": { "type": "object", "additionalProperties": false, "properties": { "a": { "type": "string" } } },
        },
      }),
      Some(URL),
    );
    assert_eq!(schema.warnings, Vec::<String>::new());
    assert_eq!(validate_with_schema(&schema.schema, &config), Ok(()));
    assert!(validate_with_schema(&schema.schema, &json!({ "test": { "b": "b" } })).is_err());
    // (the properties went to the schema the references lead to)
    let plugin = &schema.schema["$defs"]["plugin:test"];
    assert!(plugin.get("properties").is_none());
    assert!(plugin["definitions"]["closed"]["properties"]["locked"].is_object());

    // and of one that combines schemas that each say what properties it has
    let schema = build(
      json!({
        "allOf": [{ "$ref": "#/definitions/closed" }, { "properties": { "b": { "type": "string" } } }],
        "anyOf": [{ "additionalProperties": false, "properties": { "a": { "type": "string" } } }, { "required": ["b"] }],
        "definitions": {
          "closed": { "additionalProperties": false, "properties": { "a": { "type": "string" } } },
        },
      }),
      Some(URL),
    );
    // (the `anyOf` branch's properties can't be checked in an override)
    assert_eq!(schema.warnings.len(), 1, "{:?}", schema.warnings);
    assert!(
      schema.warnings[0].contains("on a condition too (`anyOf` at /anyOf/0)"),
      "{}",
      schema.warnings[0]
    );
    assert_eq!(validate_with_schema(&schema.schema, &config), Ok(()));
    // a schema that doesn't say what properties the table has stays as is
    assert_eq!(schema.schema["$defs"]["plugin:test"]["anyOf"][1], json!({ "required": ["b"] }));

    // a reference to itself is followed once
    let schema = build(json!({ "$ref": "#" }), Some(URL));
    assert_eq!(schema.warnings, Vec::<String>::new());

    // in 2020-12, what's next to a `$ref` applies too
    let schema = build(
      json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": "#/$defs/closed",
        "properties": { "b": { "type": "number" } },
        "$defs": { "closed": { "additionalProperties": false, "properties": { "a": { "type": "string" } } } },
      }),
      Some(URL),
    );
    assert_eq!(schema.warnings, Vec::<String>::new());
    assert_eq!(
      validate_with_schema(
        &schema.schema,
        &json!({ "test": { "a": "a", "associations": "**/*.a", "overrides": [{ "files": "*.a", "b": 1 }] } })
      ),
      Ok(())
    );
    assert!(validate_with_schema(&schema.schema, &json!({ "test": { "b": "one" } })).is_err());
    assert!(validate_with_schema(&schema.schema, &json!({ "test": { "overrides": [{ "files": "*.a", "b": "one" }] } })).is_err());
  }

  #[test]
  fn keeps_dprints_table_contract_whatever_the_plugins_schema_says() {
    let object_schema = json!({ "type": "object", "properties": { "a": { "type": "string" } }, "additionalProperties": false });
    let dprints_properties = json!({ "associations": "**/*.x", "locked": true, "overrides": [{ "files": "*.x", "a": "b" }] });
    for plugin_schema in [json!(true), json!({}), object_schema.clone(), json!(false)] {
      let schema = build(plugin_schema.clone(), Some(URL));
      assert_eq!(schema.warnings, Vec::<String>::new(), "{}", plugin_schema);
      let validate = |table: Value| validate_with_schema(&schema.schema, &json!({ "test": table }));
      // only an object is a plugin table, however little the plugin's schema says
      for not_a_table in [json!(123), json!("a"), json!([]), json!(null), json!(true)] {
        assert!(validate(not_a_table.clone()).is_err(), "{} {}", plugin_schema, not_a_table);
      }
      assert_eq!(validate(json!({})), Ok(()), "{}", plugin_schema);
      // with dprint's own properties, as dprint describes them (an override
      // sets a property of the plugin's, which `false` has none of)
      if plugin_schema == json!(false) {
        assert_eq!(validate(json!({ "associations": "**/*.x", "locked": true })), Ok(()));
        assert!(validate(dprints_properties.clone()).is_err());
      } else {
        assert_eq!(validate(dprints_properties.clone()), Ok(()), "{}", plugin_schema);
      }
      assert!(validate(json!({ "locked": "yes" })).is_err(), "{}", plugin_schema);
      assert!(validate(json!({ "associations": 5 })).is_err(), "{}", plugin_schema);
      assert!(validate(json!({ "overrides": [{ "a": "b" }] })).is_err(), "{}", plugin_schema);
    }

    // and the plugin's own properties, by its schema
    let allows = |plugin_schema: &Value, table: Value| validate_with_schema(&build(plugin_schema.clone(), Some(URL)).schema, &json!({ "test": table })).is_ok();
    for open in [json!(true), json!({})] {
      assert!(allows(&open, json!({ "a": 1, "zzz": true })), "{}", open);
    }
    assert!(allows(&object_schema, json!({ "a": "x" })));
    assert!(!allows(&object_schema, json!({ "a": 1 })));
    assert!(!allows(&object_schema, json!({ "zzz": true })));
    // a plugin whose schema is `false` takes no configuration of its own
    assert!(!allows(&json!(false), json!({ "a": "x" })));
    assert_eq!(
      build(json!(false), Some(URL)).schema["$defs"]["plugin:test"]["additionalProperties"],
      json!(false)
    );
  }

  #[test]
  fn warns_when_a_tables_schema_is_in_another_document() {
    let schema = build(json!({ "$ref": "https://example.com/config.json" }), Some(URL));
    assert_eq!(
      schema.warnings,
      vec![format!(
        "The configuration schema of the test plugin ({}) refers to https://example.com/config.json for its table, so editors may report dprint's own properties of its table (ex. `associations`) as unknown.",
        URL
      )]
    );
    // (which stays as it is, relative to the plugin's schema)
    let schema = build(json!({ "$ref": "../common.json" }), Some(URL));
    assert_eq!(schema.warnings.len(), 1);
    assert_eq!(schema.schema["$defs"]["plugin:test"]["$ref"], json!("../common.json"));
  }

  #[test]
  fn doesnt_apply_a_schema_that_describes_the_table_as_a_whole() {
    // what the plugin gets is its table without dprint's properties, which
    // a keyword about the object as a whole can't tell
    let host_only = json!({ "associations": "**/*.x" });
    let warning = |keyword: &str, at: &str| {
      format!(
        concat!(
          "The configuration schema of the test plugin (https://plugins.dprint.dev/test/schema.json) describes its table as a whole (`{}`{}), ",
          "which can't say what it does of the table dprint hands the plugin (without `associations`, `locked` and `overrides`), ",
          "so it isn't applied and editors don't check the plugin's options."
        ),
        keyword, at
      )
    };
    for (plugin_schema, keyword, at) in [
      (json!({ "type": "object", "maxProperties": 0 }), "maxProperties", ""),
      (json!({ "type": "object", "minProperties": 1 }), "minProperties", ""),
      (json!({ "propertyNames": { "pattern": "^[a-z]+$" } }), "propertyNames", ""),
      (json!({ "patternProperties": { "^a": { "type": "string" } } }), "patternProperties", ""),
      (json!({ "enum": [{ "a": "x" }] }), "enum", ""),
      (json!({ "const": {} }), "const", ""),
      // wherever it's applied to the table
      (
        json!({ "allOf": [{ "type": "object" }, { "maxProperties": 3 }] }),
        "maxProperties",
        " at /allOf/1",
      ),
      (
        json!({ "$ref": "#/definitions/x", "definitions": { "x": { "minProperties": 1 } } }),
        "minProperties",
        " at /definitions/x",
      ),
      (
        json!({ "if": { "minProperties": 1 }, "then": { "required": ["a"] } }),
        "minProperties",
        " at /if",
      ),
      (
        json!({ "dependencies": { "a": { "maxProperties": 2 } } }),
        "maxProperties",
        " at /dependentSchemas/a",
      ),
      (json!({ "not": { "propertyNames": { "const": "a" } } }), "propertyNames", " at /not"),
    ] {
      let schema = build(plugin_schema.clone(), Some(URL));
      assert_eq!(schema.warnings, vec![warning(keyword, at)], "{}", plugin_schema);
      // the table is dprint's, which the plugin's schema doesn't describe
      assert_eq!(
        schema.schema["properties"]["test"],
        json!({
          "description": "The configuration of the test plugin.",
          "type": "object",
          "properties": schema.schema["$defs"]["pluginTable"]["properties"],
        }),
        "{}",
        plugin_schema
      );
      assert!(schema.schema["$defs"].get("plugin:test").is_none(), "{}", plugin_schema);
      let validate = |table: &Value| validate_with_schema(&schema.schema, &json!({ "test": table }));
      // `maxProperties: 0` accepts it, as the plugin gets `{}`, and `minProperties: 1`
      // would reject it, which this can't tell: neither is applied
      assert_eq!(validate(&host_only), Ok(()), "{}", plugin_schema);
      assert_eq!(validate(&json!({ "a": 1 })), Ok(()), "{}", plugin_schema);
      assert!(validate(&json!({ "locked": "yes" })).is_err(), "{}", plugin_schema);
    }
    // in a property's schema, it's about that property's value, not the table
    let schema = build(json!({ "properties": { "a": { "type": "object", "maxProperties": 0 } } }), Some(URL));
    assert_eq!(schema.warnings, Vec::<String>::new());
    assert!(validate_with_schema(&schema.schema, &json!({ "test": { "a": { "b": 1 } } })).is_err());
  }

  #[test]
  fn checks_a_plugins_properties_in_its_overrides() {
    let validate = |plugin_schema: Value, table: Value| validate_with_schema(&build(plugin_schema, Some(URL)).schema, &json!({ "test": table }));
    let closed = json!({
      "type": "object",
      "properties": { "a": { "type": "string" }, "b": { "type": "number" } },
      "required": ["a"],
      "additionalProperties": false,
    });
    assert_eq!(
      validate(closed.clone(), json!({ "a": "x", "overrides": [{ "files": "*.x", "a": "y", "b": 1 }] })),
      Ok(())
    );
    // a property as the plugin's schema has it
    assert!(validate(closed.clone(), json!({ "a": "x", "overrides": [{ "files": "*.x", "a": 1 }] })).is_err());
    assert!(validate(closed.clone(), json!({ "a": "x", "overrides": { "files": "*.x", "b": "one" } })).is_err());
    // and no others, when the plugin's schema allows none
    assert!(validate(closed.clone(), json!({ "a": "x", "overrides": [{ "files": "*.x", "zzz": true }] })).is_err());
    // nothing is required of an override, as it's merged into the table
    assert_eq!(validate(closed.clone(), json!({ "a": "x", "overrides": [{ "files": "*.x", "b": 2 }] })), Ok(()));
    // while the table still has to have it
    assert!(validate(closed.clone(), json!({ "overrides": [{ "files": "*.x", "a": "y" }] })).is_err());
    // `files` is dprint's, and an override sets something
    assert!(validate(closed.clone(), json!({ "a": "x", "overrides": [{ "a": "y" }] })).is_err());
    assert!(validate(closed.clone(), json!({ "a": "x", "overrides": [{ "files": "*.x" }] })).is_err());
    assert!(validate(closed.clone(), json!({ "a": "x", "overrides": [{ "files": 1, "a": "y" }] })).is_err());

    // what the plugin's schema allows of other properties applies too
    let typed_others = json!({ "properties": { "a": { "type": "string" } }, "additionalProperties": { "type": "boolean" } });
    assert_eq!(
      validate(typed_others.clone(), json!({ "overrides": [{ "files": "*.x", "a": "y", "zzz": true }] })),
      Ok(())
    );
    assert!(validate(typed_others.clone(), json!({ "overrides": [{ "files": "*.x", "zzz": 1 }] })).is_err());
    // or anything, when it says nothing
    for open in [json!(true), json!({}), json!({ "type": "object" })] {
      assert_eq!(
        validate(open.clone(), json!({ "overrides": [{ "files": "*.x", "zzz": 1 }] })),
        Ok(()),
        "{}",
        open
      );
    }

    // the properties of every schema the plugin's applies to the table unconditionally
    let combined = json!({
      "$ref": "#/definitions/root",
      "definitions": {
        "root": { "allOf": [{ "$ref": "#/definitions/one" }, { "properties": { "b": { "type": "number" } }, "additionalProperties": false }] },
        "one": { "properties": { "a": { "type": "string" } } },
      },
    });
    let schema = build(combined.clone(), Some(URL));
    assert_eq!(schema.warnings, Vec::<String>::new());
    let override_schema = &schema.schema["$defs"]["plugin:test"]["$defs"]["dprint-override"];
    assert_eq!(
      override_schema["properties"]["a"],
      json!({ "$ref": "https://plugins.dprint.dev/test/schema.json#/definitions/one/properties/a" })
    );
    assert_eq!(
      override_schema["properties"]["b"],
      json!({ "$ref": "https://plugins.dprint.dev/test/schema.json#/definitions/root/allOf/1/properties/b" })
    );
    assert_eq!(override_schema["additionalProperties"], json!(false));
    assert_eq!(
      validate(combined.clone(), json!({ "overrides": [{ "files": "*.x", "a": "y", "b": 1 }] })),
      Ok(())
    );
    assert!(validate(combined.clone(), json!({ "overrides": [{ "files": "*.x", "a": 1 }] })).is_err());
    // (a property declared twice is what both say)
    let twice = json!({ "allOf": [{ "properties": { "a": { "type": "string" } } }, { "properties": { "a": { "minLength": 2 } } }] });
    assert_eq!(validate(twice.clone(), json!({ "overrides": [{ "files": "*.x", "a": "yy" }] })), Ok(()));
    assert!(validate(twice.clone(), json!({ "overrides": [{ "files": "*.x", "a": "y" }] })).is_err());
    assert!(validate(twice.clone(), json!({ "overrides": [{ "files": "*.x", "a": 22 }] })).is_err());
    // a 2020-12 schema that evaluates what its properties leave
    let unevaluated = json!({
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "properties": { "a": { "type": "string" } },
      "unevaluatedProperties": false,
    });
    assert_eq!(
      validate(
        unevaluated.clone(),
        json!({ "a": "x", "locked": true, "overrides": [{ "files": "*.x", "a": "y" }] })
      ),
      Ok(())
    );
    assert!(validate(unevaluated.clone(), json!({ "zzz": 1 })).is_err());
    assert!(validate(unevaluated.clone(), json!({ "overrides": [{ "files": "*.x", "zzz": 1 }] })).is_err());
  }

  #[test]
  fn warns_that_an_override_isnt_checked_against_what_a_schema_says_on_a_condition() {
    // the table an override is merged into decides the condition, so what
    // the schema says then isn't in the override's schema: an override
    // the plugin would reject once merged validates
    let validate = |plugin_schema: &Value, table: Value| validate_with_schema(&build(plugin_schema.clone(), Some(URL)).schema, &json!({ "test": table }));
    let warning = |keyword: &str, at: &str| {
      format!(
        concat!(
          "The configuration schema of the test plugin (https://plugins.dprint.dev/test/schema.json) says what properties its table may have on a condition too ",
          "(`{}` at {}), which an override can't be checked on by itself, as the table it's merged into decides it, so editors check an override's ",
          "properties only as far as the schema says unconditionally."
        ),
        keyword, at
      )
    };
    let either = json!({ "anyOf": [{ "properties": { "a": { "type": "string" } }, "required": ["a"] }, { "properties": { "b": { "type": "number" } }, "required": ["b"] }] });
    let schema = build(either.clone(), Some(URL));
    assert_eq!(schema.warnings, vec![warning("anyOf", "/anyOf/0")]);
    assert_eq!(
      schema.schema["$defs"]["plugin:test"]["$defs"]["dprint-override"]["properties"]
        .as_object()
        .unwrap()
        .len(),
      1
    );
    // the table itself is checked in full
    assert_eq!(validate(&either, json!({ "a": "x" })), Ok(()));
    assert!(validate(&either, json!({ "a": 1 })).is_err());
    // the override isn't: the plugin would reject `a: 1` once it's merged in
    assert_eq!(validate(&either, json!({ "a": "x", "overrides": [{ "files": "*.x", "a": 1 }] })), Ok(()));

    for (plugin_schema, keyword, at) in [
      (
        json!({ "oneOf": [{ "required": ["a"] }, { "properties": { "b": { "type": "number" } } }] }),
        "oneOf",
        "/oneOf/1",
      ),
      (
        json!({ "if": { "required": ["a"] }, "then": { "properties": { "b": { "type": "number" } } } }),
        "then",
        "/then",
      ),
      (
        json!({ "if": { "required": ["a"] }, "else": { "additionalProperties": false } }),
        "else",
        "/else",
      ),
      (json!({ "not": { "properties": { "a": { "const": "no" } } } }), "not", "/not"),
      (
        json!({ "dependencies": { "a": { "properties": { "b": { "type": "number" } } } } }),
        "dependentSchemas",
        "/dependentSchemas/a",
      ),
      // through what a branch refers to, or combines
      (
        json!({ "anyOf": [{ "$ref": "#/definitions/x" }], "definitions": { "x": { "properties": { "a": { "type": "string" } } } } }),
        "anyOf",
        "/definitions/x",
      ),
      (
        json!({ "anyOf": [{ "allOf": [{ "properties": { "a": { "type": "string" } } }] }] }),
        "anyOf",
        "/anyOf/0/allOf/0",
      ),
    ] {
      assert_eq!(
        build(plugin_schema.clone(), Some(URL)).warnings,
        vec![warning(keyword, at)],
        "{}",
        plugin_schema
      );
    }
    // a condition that says nothing about the properties' values is fine
    for plugin_schema in [
      json!({ "anyOf": [{ "required": ["a"] }, { "required": ["b"] }] }),
      json!({ "if": { "required": ["a"] }, "then": { "required": ["b"] }, "else": { "additionalProperties": true } }),
      json!({ "properties": { "a": { "anyOf": [{ "properties": { "x": { "type": "string" } } }, { "type": "string" }] } } }),
    ] {
      assert_eq!(build(plugin_schema.clone(), Some(URL)).warnings, Vec::<String>::new(), "{}", plugin_schema);
    }
  }

  #[test]
  fn embeds_a_schema_of_every_draft_it_knows() {
    // each in its own syntax, which a validator of the configuration schema
    // (2020-12) checks the same way
    let table = json!({ "pair": ["a", 1], "n": 2, "a": "x", "overrides": [{ "files": "*.x", "n": 3 }] });
    for (dialect, plugin_schema) in [
      (
        "http://json-schema.org/draft-04/schema#",
        json!({
          "id": "https://example.com/v4.json",
          "properties": {
            "pair": { "type": "array", "items": [{ "type": "string" }, { "type": "integer" }], "additionalItems": false },
            "n": { "type": "integer", "minimum": 1, "exclusiveMinimum": true },
            "a": { "$ref": "#/definitions/a" },
          },
          "definitions": { "a": { "type": "string" } },
        }),
      ),
      (
        "http://json-schema.org/draft-06/schema#",
        json!({
          "properties": {
            "pair": { "type": "array", "items": [{ "type": "string" }, { "type": "integer" }], "additionalItems": false },
            "n": { "type": "integer", "exclusiveMinimum": 1 },
            "a": { "$ref": "#a", "type": "number" },
          },
          "definitions": { "a": { "$id": "#a", "type": "string" } },
        }),
      ),
      (
        "http://json-schema.org/draft-07/schema#",
        json!({
          "properties": {
            "pair": { "type": "array", "items": [{ "type": "string" }, { "type": "integer" }], "additionalItems": false },
            "n": { "type": "integer", "exclusiveMinimum": 1 },
            "a": { "$ref": "#a" },
          },
          "dependencies": { "a": ["n"] },
          "definitions": { "a": { "$id": "#a", "type": "string" } },
        }),
      ),
      (
        "https://json-schema.org/draft/2019-09/schema",
        json!({
          "properties": {
            "pair": { "type": "array", "items": [{ "type": "string" }, { "type": "integer" }], "additionalItems": false },
            "n": { "type": "integer", "exclusiveMinimum": 1 },
            "a": { "$ref": "#a" },
          },
          "dependentRequired": { "a": ["n"] },
          "$defs": { "a": { "$anchor": "a", "type": "string" } },
        }),
      ),
      (
        "https://json-schema.org/draft/2020-12/schema",
        json!({
          "properties": {
            "pair": { "type": "array", "prefixItems": [{ "type": "string" }, { "type": "integer" }], "items": false },
            "n": { "type": "integer", "exclusiveMinimum": 1 },
            "a": { "$ref": "#a" },
          },
          "dependentRequired": { "a": ["n"] },
          "$defs": { "a": { "$anchor": "a", "type": "string" } },
        }),
      ),
    ] {
      let mut plugin_schema = plugin_schema;
      plugin_schema["$schema"] = json!(dialect);
      let schema = build(plugin_schema.clone(), Some(URL));
      assert_eq!(schema.warnings, Vec::<String>::new(), "{}", dialect);
      let plugin = &schema.schema["$defs"]["plugin:test"];
      assert!(plugin.get("$schema").is_none(), "{}", dialect);
      assert_eq!(plugin["properties"]["pair"]["prefixItems"].as_array().map(Vec::len), Some(2), "{}", dialect);
      assert_eq!(plugin["properties"]["n"]["exclusiveMinimum"], json!(1), "{}", dialect);
      let validate = |table: Value| validate_with_schema(&schema.schema, &json!({ "test": table }));
      assert_eq!(validate(table.clone()), Ok(()), "{}", dialect);
      assert!(validate(json!({ "pair": [1, "a"] })).is_err(), "{}", dialect);
      assert!(validate(json!({ "pair": ["a", 1, true] })).is_err(), "{}", dialect);
      assert!(validate(json!({ "n": 1 })).is_err(), "{}", dialect);
      assert!(validate(json!({ "a": 1 })).is_err(), "{}", dialect);
      assert!(validate(json!({ "overrides": [{ "files": "*.x", "n": 0 }] })).is_err(), "{}", dialect);
    }
    // (a schema without a `$schema` is taken to be draft-07)
    let schema = build(
      json!({ "properties": { "a": { "$ref": "#/definitions/a", "type": "number" } }, "definitions": { "a": { "type": "string" } } }),
      Some(URL),
    );
    assert_eq!(validate_with_schema(&schema.schema, &json!({ "test": { "a": "x" } })), Ok(()));
  }

  #[test]
  fn refers_to_a_schema_of_an_unknown_draft() {
    for dialect in ["https://json-schema.org/draft/2099-01/schema", "https://example.com/my-dialect"] {
      let schema = build(json!({ "$schema": dialect, "properties": { "a": { "type": "string" } } }), Some(URL));
      // dprint's table, which that schema applies to as well
      assert_eq!(schema.schema["properties"]["test"], table_schema("test", URL, false));
      assert!(schema.schema["$defs"].get("plugin:test").is_none());
      assert_eq!(
        schema.warnings,
        vec![format!(
          concat!(
            "The configuration schema of the test plugin ({}) is for {}, a JSON schema draft dprint doesn't know, so it's referred to by its url instead. ",
            "Editors may report dprint's own properties of its table (ex. `associations`) as unknown."
          ),
          URL, dialect
        )]
      );
    }
    // and one built into dprint is left out
    let schema = build(json!({ "$schema": "https://example.com/my-dialect" }), None);
    assert!(schema.schema["properties"].get("test").is_none());
    assert_eq!(schema.warnings.len(), 1);

    // a schema within of such a draft refuses the whole schema the same way:
    // it can't be translated, nor read in the root's dialect
    let schema = build(
      json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "properties": { "a": { "$ref": "https://example.com/custom.json" }, "b": { "type": "string" } },
        "definitions": {
          "custom": {
            "$id": "https://example.com/custom.json",
            "$schema": "https://example.com/my-dialect",
            "items": [{ "type": "string" }],
            "x-opaque": { "$ref": "whatever" },
          },
        },
      }),
      Some(URL),
    );
    assert_eq!(schema.schema["properties"]["test"], table_schema("test", URL, false));
    assert!(schema.schema["$defs"].get("plugin:test").is_none());
    assert_eq!(
      schema.warnings,
      vec![format!(
        concat!(
          "The configuration schema of the test plugin ({}) is for https://example.com/my-dialect (at /definitions/custom), a JSON schema draft dprint doesn't know, ",
          "so it's referred to by its url instead. Editors may report dprint's own properties of its table (ex. `associations`) as unknown."
        ),
        URL
      )]
    );
  }
}
