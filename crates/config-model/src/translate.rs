//! Plugin schemas of any JSON schema draft as 2020-12 schema resources.
//!
//! JSON schema 2020-12 bundles schemas by embedding each as a resource that
//! keeps its `$id` and its references, so a plugin's schema isn't rewritten
//! to be part of the configuration schema. What's translated is the syntax
//! an earlier draft had for the same thing (see [`to_2020_12`]), as the
//! validators that check a configuration file read the whole document in
//! its root's dialect.

use std::collections::HashMap;

use serde_json::Map;
use serde_json::Value;
use url::Url;

use crate::pointer;

/// A JSON schema dialect (draft) dprint can make a 2020-12 resource of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
  Draft04,
  Draft06,
  Draft07,
  Draft2019_09,
  Draft2020_12,
}

impl Dialect {
  /// What a schema without a `$schema` is taken to be: draft-07, which
  /// dprint asked plugins for before 2020-12.
  pub const DEFAULT: Self = Self::Draft07;

  /// The dialect a `$schema` names, if it's one dprint knows.
  pub fn of(uri: &str) -> Option<Self> {
    let uri = uri.trim_end_matches('#');
    let path = uri.strip_prefix("https://").or_else(|| uri.strip_prefix("http://")).unwrap_or(uri);
    match path {
      "json-schema.org/draft-04/schema" => Some(Self::Draft04),
      "json-schema.org/draft-06/schema" => Some(Self::Draft06),
      "json-schema.org/draft-07/schema" => Some(Self::Draft07),
      "json-schema.org/draft/2019-09/schema" => Some(Self::Draft2019_09),
      "json-schema.org/draft/2020-12/schema" => Some(Self::Draft2020_12),
      _ => None,
    }
  }

  /// Whether what's next to a `$ref` is ignored (until 2019-09 it was).
  fn ignores_next_to_ref(self) -> bool {
    matches!(self, Self::Draft04 | Self::Draft06 | Self::Draft07)
  }
}

/// Makes a schema of `dialect`, the resource at `uri`, a 2020-12 resource:
///
/// - a `$schema` within it is dropped (the root's is the configuration
///   schema's), as is draft-04's `id` for `$id`;
/// - an `$id` that names a schema within its resource (`"$id": "#name"`,
///   which drafts before 2019-09 had for an anchor) becomes an `$anchor`,
///   and one that starts a resource is made absolute;
/// - draft-04's boolean `exclusiveMinimum` and `exclusiveMaximum` become
///   the numbers;
/// - an `items` array becomes `prefixItems`, with `additionalItems` as its
///   `items`;
/// - `dependencies` becomes `dependentRequired` and `dependentSchemas`;
/// - 2019-09's `$recursiveRef` and `$recursiveAnchor` become `$dynamicRef`
///   and `$dynamicAnchor`;
/// - what's next to a `$ref` other than annotations, which the drafts
///   before 2019-09 ignore, is dropped, as 2020-12 would apply it.
///
/// The references stay as they are: they're resolved against the `$id`s,
/// as in the plugin's own document.
///
/// A schema within that declares a dialect dprint doesn't know can't be
/// translated, nor left as it is (a validator would read it in the root's
/// dialect), so the whole schema is refused as [`UnknownDialect`], saying
/// where. The schema is then partly translated and not to be used.
pub fn to_2020_12(schema: &mut Value, dialect: Dialect, uri: &Url) -> Result<(), UnknownDialect> {
  translate(schema, dialect, uri, "")?;
  if let Value::Object(object) = schema {
    object.shift_remove("$id");
    let mut root = Map::new();
    root.insert("$id".to_string(), Value::String(uri.to_string()));
    root.extend(std::mem::take(object));
    *object = root;
  }
  Ok(())
}

/// A `$schema` of a draft dprint doesn't know, and where it is (the JSON
/// pointer of the schema it's in; empty for the root).
#[derive(Debug, PartialEq, Eq)]
pub struct UnknownDialect {
  pub dialect: String,
  pub pointer: String,
}

/// The keywords a draft before 2019-09 ignores next to a `$ref`: all but
/// annotations (ex. `description`), the definitions references point into,
/// and what isn't a keyword of the draft (ex. an editor's `x-` extension).
const IGNORED_NEXT_TO_REF: &[&str] = &[
  "$id",
  "id",
  "$schema",
  "type",
  "enum",
  "const",
  "multipleOf",
  "maximum",
  "exclusiveMaximum",
  "minimum",
  "exclusiveMinimum",
  "maxLength",
  "minLength",
  "pattern",
  "items",
  "additionalItems",
  "maxItems",
  "minItems",
  "uniqueItems",
  "contains",
  "maxProperties",
  "minProperties",
  "required",
  "properties",
  "patternProperties",
  "additionalProperties",
  "dependencies",
  "propertyNames",
  "if",
  "then",
  "else",
  "allOf",
  "anyOf",
  "oneOf",
  "not",
  "format",
  "contentMediaType",
  "contentEncoding",
];

fn translate(schema: &mut Value, dialect: Dialect, base: &Url, pointer: &str) -> Result<(), UnknownDialect> {
  let Value::Object(object) = schema else {
    return Ok(());
  };
  // a `$schema` within switches the dialect for what's under it (the
  // root's was checked, and is the caller's `dialect`)
  let dialect = match object.get("$schema") {
    Some(Value::String(uri)) if !pointer.is_empty() => Dialect::of(uri).ok_or_else(|| UnknownDialect {
      dialect: uri.clone(),
      pointer: pointer.to_string(),
    })?,
    Some(other) if !pointer.is_empty() => {
      return Err(UnknownDialect {
        dialect: other.to_string(),
        pointer: pointer.to_string(),
      });
    }
    _ => dialect,
  };
  object.shift_remove("$schema");
  if dialect.ignores_next_to_ref() && object.contains_key("$ref") {
    object.retain(|key, _| !IGNORED_NEXT_TO_REF.contains(&key.as_str()));
  }
  if dialect == Dialect::Draft04 {
    if let Some(id) = object.shift_remove("id") {
      object.insert("$id".to_string(), id);
    }
    for (exclusive, limit) in [("exclusiveMinimum", "minimum"), ("exclusiveMaximum", "maximum")] {
      if let Some(Value::Bool(is_exclusive)) = object.get(exclusive) {
        let number = if *is_exclusive { object.shift_remove(limit) } else { None };
        object.shift_remove(exclusive);
        if let Some(number) = number {
          object.insert(exclusive.to_string(), number);
        }
      }
    }
  }
  // the `$id`: an anchor, a resource of its own, or nothing
  let base = match object.shift_remove("$id") {
    Some(Value::String(id)) => {
      let id = SchemaId::new(&id, base);
      if let Some(anchor) = id.anchor {
        object.insert("$anchor".to_string(), Value::String(anchor));
      }
      match id.resource {
        Some(resource) => {
          object.insert("$id".to_string(), Value::String(resource.to_string()));
          resource
        }
        None => base.clone(),
      }
    }
    _ => base.clone(),
  };
  if dialect != Dialect::Draft2020_12 {
    if let Some(Value::Array(_)) = object.get("items") {
      let items = object.shift_remove("items").unwrap();
      object.insert("prefixItems".to_string(), items);
      if let Some(additional) = object.shift_remove("additionalItems") {
        object.insert("items".to_string(), additional);
      }
    } else {
      object.shift_remove("additionalItems");
    }
  }
  if dialect.ignores_next_to_ref()
    && let Some(Value::Object(dependencies)) = object.shift_remove("dependencies")
  {
    for (name, dependency) in dependencies {
      let keyword = if dependency.is_array() { "dependentRequired" } else { "dependentSchemas" };
      let Value::Object(entries) = object.entry(keyword).or_insert_with(|| Value::Object(Map::new())) else {
        continue;
      };
      entries.entry(name).or_insert(dependency);
    }
  }
  if dialect == Dialect::Draft2019_09 {
    if let Some(Value::Bool(true)) = object.shift_remove("$recursiveAnchor") {
      object.insert("$dynamicAnchor".to_string(), Value::String("meta".to_string()));
    }
    if let Some(Value::String(reference)) = object.shift_remove("$recursiveRef") {
      let reference = if reference == "#" { "#meta".to_string() } else { reference };
      object.insert("$dynamicRef".to_string(), Value::String(reference));
    }
  }
  for (keyword, value) in object.iter_mut() {
    let keyword_pointer = pointer::append(pointer, keyword);
    match keyword.as_str() {
      // property names (or patterns, or definition names) to schemas
      "properties" | "patternProperties" | "definitions" | "$defs" | "dependentSchemas" => {
        if let Value::Object(schemas) = value {
          for (name, schema) in schemas.iter_mut() {
            translate(schema, dialect, &base, &pointer::append(&keyword_pointer, name))?;
          }
        }
      }
      // a schema or a list of them
      "items"
      | "prefixItems"
      | "unevaluatedItems"
      | "contains"
      | "additionalProperties"
      | "unevaluatedProperties"
      | "propertyNames"
      | "contentSchema"
      | "if"
      | "then"
      | "else"
      | "allOf"
      | "anyOf"
      | "oneOf"
      | "not" => match value {
        Value::Array(schemas) => {
          for (index, schema) in schemas.iter_mut().enumerate() {
            translate(schema, dialect, &base, &format!("{}/{}", keyword_pointer, index))?;
          }
        }
        value => translate(value, dialect, &base, &keyword_pointer)?,
      },
      // data (ex. `default`), or a keyword dprint doesn't know, which an
      // extension may use for anything (ex. an `x-tool` with a `$ref`)
      _ => {}
    }
  }
  Ok(())
}

/// What a schema's `$id` says.
#[derive(Default)]
struct SchemaId {
  /// The uri of the resource it starts, when it starts one, which its
  /// references are relative to.
  resource: Option<Url>,
  /// The name it gives the schema within its resource (an anchor).
  anchor: Option<String>,
}

impl SchemaId {
  fn new(id: &str, base: &Url) -> Self {
    let Ok(url) = base.join(id) else {
      return Self::default();
    };
    let anchor = url
      .fragment()
      .map(pointer::decode_fragment)
      .filter(|fragment| !fragment.is_empty() && !fragment.starts_with('/'));
    let mut resource = url;
    resource.set_fragment(None);
    Self {
      anchor,
      // an `$id` with the current resource's uri (ex. a fragment, or an
      // absolute uri with a fragment) names a schema in it rather than
      // starting another
      resource: (resource != *base).then_some(resource),
    }
  }
}

/// Where the resources (schemas with an `$id`) and anchors within a 2020-12
/// resource are, for resolving its references the way a validator does.
pub struct ResourceIndex {
  /// By uri, the JSON pointer of each resource: the root (`""`) and each
  /// subschema with an `$id` of its own.
  resources: HashMap<Url, String>,
  /// By pointer, the uri of each resource.
  bases: Vec<(String, Url)>,
  /// By uri and name, the JSON pointer of each anchor.
  anchors: HashMap<(Url, String), String>,
}

impl ResourceIndex {
  pub fn of(schema: &Value, uri: &Url) -> Self {
    let mut index = Self {
      resources: HashMap::from([(uri.clone(), String::new())]),
      bases: vec![(String::new(), uri.clone())],
      anchors: HashMap::new(),
    };
    walk(schema, &mut |object, pointer| {
      if !pointer.is_empty()
        && let Some(Value::String(id)) = object.get("$id")
        && let Ok(id) = Url::parse(id)
      {
        index.resources.insert(id.clone(), pointer.to_string());
        index.bases.push((pointer.to_string(), id));
      }
      for keyword in ["$anchor", "$dynamicAnchor"] {
        if let Some(Value::String(name)) = object.get(keyword) {
          let base = index.base_of(pointer).clone();
          index.anchors.insert((base, name.clone()), pointer.to_string());
        }
      }
    });
    index
  }

  /// The nearest resource the schema at `pointer` is in: its pointer and uri.
  fn resource_of(&self, pointer: &str) -> (&str, &Url) {
    self
      .bases
      .iter()
      .filter(|(resource, _)| pointer == resource || pointer.starts_with(&format!("{}/", resource)) || resource.is_empty())
      .max_by_key(|(resource, _)| resource.len())
      .map(|(resource, uri)| (resource.as_str(), uri))
      .expect("the root is a resource")
  }

  /// The uri of the resource the schema at `pointer` is in.
  fn base_of(&self, pointer: &str) -> &Url {
    self.resource_of(pointer).1
  }

  /// The JSON pointer within the resource of what a `$ref` at `pointer`
  /// refers to. `None` for a reference to another document (or to an
  /// anchor the resource doesn't have).
  pub fn resolve(&self, reference: &str, pointer: &str) -> Option<String> {
    let target = self.base_of(pointer).join(reference).ok()?;
    let fragment = target.fragment().map(pointer::decode_fragment).unwrap_or_default();
    let mut resource = target;
    resource.set_fragment(None);
    let resource_pointer = self.resources.get(&resource)?;
    if fragment.is_empty() || fragment.starts_with('/') {
      return Some(format!("{}{}", resource_pointer, fragment));
    }
    self.anchors.get(&(resource, fragment)).cloned()
  }

  /// A reference to the schema at `pointer` from anywhere: the uri of the
  /// resource it's in, with its JSON pointer within that.
  pub fn reference_to(&self, pointer: &str) -> String {
    let (resource, uri) = self.resource_of(pointer);
    pointer::resource_reference(uri.as_str(), &pointer[resource.len()..])
  }
}

/// Calls `visit` with each schema within `schema` (itself included) and its
/// JSON pointer, parents first.
///
/// Only schemas are visited: the values of the keywords that hold schemas.
/// Not the values of keywords that are data (ex. `default`), nor property
/// names (ex. a property named `$ref`), nor the values of keywords dprint
/// doesn't know, which an extension may use for anything.
pub fn walk(schema: &Value, visit: &mut dyn FnMut(&Map<String, Value>, &str)) {
  fn go(schema: &Value, pointer: &str, visit: &mut dyn FnMut(&Map<String, Value>, &str)) {
    let Value::Object(object) = schema else {
      return;
    };
    visit(object, pointer);
    for (keyword, value) in object {
      let keyword_pointer = pointer::append(pointer, keyword);
      match keyword.as_str() {
        "properties" | "patternProperties" | "definitions" | "$defs" | "dependentSchemas" => {
          if let Value::Object(schemas) = value {
            for (name, schema) in schemas {
              go(schema, &pointer::append(&keyword_pointer, name), visit);
            }
          }
        }
        "items"
        | "prefixItems"
        | "unevaluatedItems"
        | "contains"
        | "additionalProperties"
        | "unevaluatedProperties"
        | "propertyNames"
        | "contentSchema"
        | "if"
        | "then"
        | "else"
        | "allOf"
        | "anyOf"
        | "oneOf"
        | "not" => match value {
          Value::Array(schemas) => {
            for (index, schema) in schemas.iter().enumerate() {
              go(schema, &format!("{}/{}", keyword_pointer, index), visit);
            }
          }
          value => go(value, &keyword_pointer, visit),
        },
        _ => {}
      }
    }
  }
  go(schema, "", visit);
}

#[cfg(test)]
mod test {
  use pretty_assertions::assert_eq;
  use serde_json::json;

  use super::*;

  fn uri() -> Url {
    Url::parse("https://plugins.dprint.dev/test/schema.json").unwrap()
  }

  fn translated(mut schema: Value, dialect: Dialect) -> Value {
    to_2020_12(&mut schema, dialect, &uri()).unwrap();
    schema
  }

  #[test]
  fn refuses_a_schema_within_of_a_draft_it_doesnt_know() {
    // what its keywords mean can't be known, so it can't be translated, nor
    // left for a validator to read in the root's dialect
    let custom = json!({
      "$id": "https://example.com/custom.json",
      "$schema": "https://example.com/my-dialect",
      "items": [{ "type": "string" }],
      "x-opaque": { "$ref": "whatever" },
    });
    let mut schema = json!({
      "$schema": "http://json-schema.org/draft-07/schema#",
      "definitions": { "custom": custom },
      "properties": { "a": { "$ref": "https://example.com/custom.json" } },
    });
    assert_eq!(
      to_2020_12(&mut schema, Dialect::Draft07, &uri()),
      Err(UnknownDialect {
        dialect: "https://example.com/my-dialect".to_string(),
        pointer: "/definitions/custom".to_string(),
      })
    );
    // nothing of it was touched
    assert_eq!(schema["definitions"]["custom"], custom);
    // not even for a `$schema` that isn't a string
    let mut schema = json!({ "properties": { "a": { "$schema": 7 } } });
    assert_eq!(to_2020_12(&mut schema, Dialect::Draft07, &uri()).unwrap_err().pointer, "/properties/a");
    // while one of a draft it knows is translated as that draft says
    let schema = translated(
      json!({ "$defs": { "old": { "$schema": "http://json-schema.org/draft-04/schema#", "id": "https://example.com/old.json", "minimum": 1, "exclusiveMinimum": true } } }),
      Dialect::Draft2020_12,
    );
    assert_eq!(schema["$defs"]["old"], json!({ "$id": "https://example.com/old.json", "exclusiveMinimum": 1 }));
  }

  #[test]
  fn knows_the_drafts() {
    assert_eq!(Dialect::of("http://json-schema.org/draft-07/schema#"), Some(Dialect::Draft07));
    assert_eq!(Dialect::of("https://json-schema.org/draft-07/schema"), Some(Dialect::Draft07));
    assert_eq!(Dialect::of("http://json-schema.org/draft-04/schema#"), Some(Dialect::Draft04));
    assert_eq!(Dialect::of("https://json-schema.org/draft/2019-09/schema"), Some(Dialect::Draft2019_09));
    assert_eq!(Dialect::of("https://json-schema.org/draft/2020-12/schema"), Some(Dialect::Draft2020_12));
    assert_eq!(Dialect::of("https://json-schema.org/draft/2099-01/schema"), None);
    assert_eq!(Dialect::of("https://example.com/my-dialect"), None);
  }

  #[test]
  fn gives_the_resource_its_uri_first() {
    let schema = translated(
      json!({ "$schema": "http://json-schema.org/draft-07/schema#", "type": "object" }),
      Dialect::Draft07,
    );
    assert_eq!(
      schema.as_object().unwrap().iter().collect::<Vec<_>>(),
      vec![(&"$id".to_string(), &json!(uri().as_str())), (&"type".to_string(), &json!("object"))]
    );
    // whatever `$id` it had, which the uri was made from
    let schema = translated(json!({ "$id": "schema.json", "type": "object" }), Dialect::Draft07);
    assert_eq!(schema["$id"], json!(uri().as_str()));
  }

  #[test]
  fn makes_anchors_of_fragment_ids_and_resources_absolute() {
    let schema = translated(
      json!({
        "definitions": {
          "color": { "$id": "#kleur", "type": "string" },
          "named": { "$id": "https://plugins.dprint.dev/test/schema.json#name", "type": "string" },
          "inner": { "$id": "inner/schema.json", "definitions": { "y": { "$id": "#y", "type": "number" } } },
          "same": { "$id": "#", "type": "string" },
        },
      }),
      Dialect::Draft07,
    );
    assert_eq!(schema["definitions"]["color"], json!({ "$anchor": "kleur", "type": "string" }));
    assert_eq!(schema["definitions"]["named"], json!({ "$anchor": "name", "type": "string" }));
    assert_eq!(
      schema["definitions"]["inner"]["$id"],
      json!("https://plugins.dprint.dev/test/inner/schema.json")
    );
    assert_eq!(schema["definitions"]["inner"]["definitions"]["y"], json!({ "$anchor": "y", "type": "number" }));
    assert_eq!(schema["definitions"]["same"], json!({ "type": "string" }));
  }

  #[test]
  fn translates_what_a_draft_said_differently() {
    let schema = translated(
      json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "properties": {
          "pair": { "type": "array", "items": [{ "type": "string" }, { "type": "integer" }], "additionalItems": false },
          "list": { "type": "array", "items": { "type": "string" }, "additionalItems": false },
          "nested": { "$schema": "https://json-schema.org/draft/2020-12/schema", "prefixItems": [{ "type": "string" }] },
        },
        "dependencies": { "a": ["b"], "c": { "required": ["d"] } },
        "dependentRequired": { "e": ["f"] },
      }),
      Dialect::Draft07,
    );
    assert_eq!(
      schema["properties"]["pair"],
      json!({ "type": "array", "prefixItems": [{ "type": "string" }, { "type": "integer" }], "items": false })
    );
    assert_eq!(schema["properties"]["list"], json!({ "type": "array", "items": { "type": "string" } }));
    assert_eq!(schema["properties"]["nested"], json!({ "prefixItems": [{ "type": "string" }] }));
    assert!(schema.get("dependencies").is_none());
    assert_eq!(schema["dependentRequired"], json!({ "e": ["f"], "a": ["b"] }));
    assert_eq!(schema["dependentSchemas"], json!({ "c": { "required": ["d"] } }));

    let schema = translated(
      json!({
        "id": "https://example.com/old.json",
        "properties": {
          "min": { "type": "number", "minimum": 1, "exclusiveMinimum": true },
          "max": { "type": "number", "maximum": 9, "exclusiveMaximum": false },
          "ref": { "$ref": "#/definitions/x", "id": "#ignored" },
        },
        "definitions": { "x": { "type": "string" } },
      }),
      Dialect::Draft04,
    );
    assert_eq!(schema["$id"], json!(uri().as_str()));
    assert!(schema.get("id").is_none());
    assert_eq!(schema["properties"]["min"], json!({ "type": "number", "exclusiveMinimum": 1 }));
    assert_eq!(schema["properties"]["max"], json!({ "type": "number", "maximum": 9 }));
    assert_eq!(schema["properties"]["ref"], json!({ "$ref": "#/definitions/x" }));

    let schema = translated(
      json!({ "$recursiveAnchor": true, "properties": { "child": { "$recursiveRef": "#" } } }),
      Dialect::Draft2019_09,
    );
    assert_eq!(schema["$dynamicAnchor"], json!("meta"));
    assert_eq!(schema["properties"]["child"], json!({ "$dynamicRef": "#meta" }));
  }

  #[test]
  fn drops_what_a_draft_ignored_next_to_a_reference() {
    let schema = translated(
      json!({
        "$ref": "#/definitions/x",
        "type": "integer",
        "description": "kept",
        "default": 1,
        "x-tool": { "$ref": "literal" },
        "definitions": { "x": { "type": "string", "$ref": "#/definitions/y", "minLength": 1 }, "y": { "type": "string" } },
      }),
      Dialect::Draft07,
    );
    assert_eq!(
      schema,
      json!({
        "$id": uri().as_str(),
        "$ref": "#/definitions/x",
        "description": "kept",
        "default": 1,
        "x-tool": { "$ref": "literal" },
        "definitions": { "x": { "$ref": "#/definitions/y" }, "y": { "type": "string" } },
      })
    );
    // a 2020-12 schema means what's next to a `$ref`
    let schema = translated(
      json!({ "properties": { "a": { "$ref": "#/$defs/x", "minLength": 1 } }, "$defs": { "x": {} } }),
      Dialect::Draft2020_12,
    );
    assert_eq!(schema["properties"]["a"], json!({ "$ref": "#/$defs/x", "minLength": 1 }));
  }

  #[test]
  fn leaves_data_and_unknown_keywords_alone() {
    let data = json!({ "$id": "#data", "$ref": "#/definitions/a", "items": [1], "dependencies": {} });
    let schema = translated(
      json!({
        "definitions": { "a": { "type": "string" } },
        "x-tool": data,
        "properties": {
          "$ref": { "$ref": "#/definitions/a" },
          "value": { "default": data, "enum": [data], "const": data, "examples": [data], "x-tool": data },
        },
      }),
      Dialect::Draft07,
    );
    assert_eq!(schema["x-tool"], data);
    assert_eq!(schema["properties"]["$ref"], json!({ "$ref": "#/definitions/a" }));
    for keyword in ["default", "const", "x-tool"] {
      assert_eq!(schema["properties"]["value"][keyword], data, "{}", keyword);
    }
    assert_eq!(schema["properties"]["value"]["enum"], json!([data]));
  }

  #[test]
  fn resolves_references_as_a_validator_does() {
    let schema = translated(
      json!({
        "definitions": {
          "color": { "$id": "#kleur", "type": "string" },
          "x": { "type": "string" },
          "inner": {
            "$id": "inner/schema.json",
            "definitions": { "y": { "$id": "#why", "type": "number" }, "z": { "type": "number" } },
          },
        },
      }),
      Dialect::Draft07,
    );
    let index = ResourceIndex::of(&schema, &uri());
    assert_eq!(index.resolve("#", ""), Some(String::new()));
    assert_eq!(index.resolve("#/definitions/x", ""), Some("/definitions/x".to_string()));
    assert_eq!(index.resolve("#/definitions/x", "/properties/deep"), Some("/definitions/x".to_string()));
    assert_eq!(index.resolve("#kleur", ""), Some("/definitions/color".to_string()));
    assert_eq!(
      index.resolve("https://plugins.dprint.dev/test/schema.json#kleur", ""),
      Some("/definitions/color".to_string())
    );
    assert_eq!(index.resolve("schema.json#/definitions/x", ""), Some("/definitions/x".to_string()));
    assert_eq!(index.resolve("inner/schema.json", ""), Some("/definitions/inner".to_string()));
    assert_eq!(index.resolve("inner/schema.json#why", ""), Some("/definitions/inner/definitions/y".to_string()));
    // within the nested resource, relative to it
    assert_eq!(
      index.resolve("#/definitions/z", "/definitions/inner/properties/p"),
      Some("/definitions/inner/definitions/z".to_string())
    );
    assert_eq!(
      index.resolve("#why", "/definitions/inner"),
      Some("/definitions/inner/definitions/y".to_string())
    );
    assert_eq!(
      index.resolve("../schema.json#/definitions/x", "/definitions/inner"),
      Some("/definitions/x".to_string())
    );
    // elsewhere
    assert_eq!(index.resolve("#elsewhere", ""), None);
    assert_eq!(index.resolve("other.json", ""), None);
    assert_eq!(index.resolve("https://example.com/schema.json#/definitions/x", ""), None);

    assert_eq!(
      index.reference_to("/definitions/x"),
      "https://plugins.dprint.dev/test/schema.json#/definitions/x"
    );
    assert_eq!(index.reference_to(""), "https://plugins.dprint.dev/test/schema.json#");
    assert_eq!(
      index.reference_to("/definitions/inner/definitions/z"),
      "https://plugins.dprint.dev/test/inner/schema.json#/definitions/z"
    );
    assert_eq!(index.reference_to("/definitions/inner"), "https://plugins.dprint.dev/test/inner/schema.json#");
    assert_eq!(
      index.reference_to("/definitions/a~1b"),
      "https://plugins.dprint.dev/test/schema.json#/definitions/a~1b"
    );
  }
}
