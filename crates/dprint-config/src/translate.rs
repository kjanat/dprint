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

/// A JSON schema dialect (draft) dprint can make a 2020-12 resource of, in
/// order of publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
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

  /// Whether a keyword that 2020-12 applies (an applicator, a validation
  /// keyword, or a core keyword that changes what a reference means) is the
  /// draft's too. One that isn't is ignored by the draft, so a 2020-12
  /// resource mustn't have it (see [`to_2020_12`]). Any other keyword is
  /// `true`: an annotation, a container of definitions, or one of no draft,
  /// which means nothing to a validator in any of them. (What a draft says
  /// differently from 2020-12, ex. draft-04's `id`, is translated by name.)
  fn knows(self, keyword: &str) -> bool {
    let since = match keyword {
      "$ref"
      | "$schema"
      | "type"
      | "enum"
      | "multipleOf"
      | "maximum"
      | "exclusiveMaximum"
      | "minimum"
      | "exclusiveMinimum"
      | "maxLength"
      | "minLength"
      | "pattern"
      | "items"
      | "maxItems"
      | "minItems"
      | "uniqueItems"
      | "maxProperties"
      | "minProperties"
      | "required"
      | "properties"
      | "patternProperties"
      | "additionalProperties"
      | "allOf"
      | "anyOf"
      | "oneOf"
      | "not" => Self::Draft04,
      "$id" | "const" | "contains" | "propertyNames" => Self::Draft06,
      "if" | "then" | "else" => Self::Draft07,
      "$anchor" | "dependentRequired" | "dependentSchemas" | "unevaluatedItems" | "unevaluatedProperties" | "maxContains" | "minContains" => Self::Draft2019_09,
      "prefixItems" | "$dynamicRef" | "$dynamicAnchor" => Self::Draft2020_12,
      _ => return true,
    };
    self >= since
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
///   before 2019-09 ignore, is dropped, as 2020-12 would apply it;
/// - so is a keyword the draft doesn't know but 2020-12 applies (ex.
///   draft-04's `if`, or draft-07's `unevaluatedProperties`), which the
///   draft ignores (see [`Dialect::knows`]).
///
/// The references stay as they are: they're resolved against the `$id`s,
/// as in the plugin's own document. Except that a reference by JSON pointer
/// into a schema the translation moved (ex. `#/definitions/pair/items/0`,
/// a tuple's item that is now under `prefixItems`) is rewritten to where
/// that schema is now, as the pointer would otherwise point at nothing or
/// at something else.
///
/// A schema within that declares a dialect dprint doesn't know can't be
/// translated, nor left as it is (a validator would read it in the root's
/// dialect), so the whole schema is refused as
/// [`Untranslatable::UnknownDialect`], saying where. So is one with a
/// reference by JSON pointer into what the translation drops
/// ([`Untranslatable::ReferenceIntoDropped`]), which can't be kept. The
/// schema is then partly translated and not to be used.
pub fn to_2020_12(schema: &mut Value, dialect: Dialect, uri: &Url) -> Result<(), Untranslatable> {
  let mut moves = Vec::new();
  translate(schema, dialect, uri, "", &mut moves)?;
  if !moves.is_empty() {
    rewrite_references_into_moved_schemas(schema, uri, &moves)?;
  }
  if let Value::Object(object) = schema {
    object.shift_remove("$id");
    let mut root = Map::new();
    root.insert("$id".to_string(), Value::String(uri.to_string()));
    root.extend(std::mem::take(object));
    *object = root;
  }
  Ok(())
}

/// Why a schema can't be made a 2020-12 resource.
#[derive(Debug, PartialEq, Eq)]
pub enum Untranslatable {
  /// A `$schema` of a draft dprint doesn't know, and where it is (the JSON
  /// pointer of the schema it's in; empty for the root).
  UnknownDialect { dialect: String, pointer: String },
  /// A `$ref` by JSON pointer into what the translation drops (a keyword
  /// the schema's draft ignores, which 2020-12 would apply), and where the
  /// reference and the dropped keyword are (JSON pointers).
  ReferenceIntoDropped { reference: String, pointer: String, dropped: String },
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

/// Where the translation moves a schema from and to, as JSON pointers from
/// the document's root (`None` for one that's dropped). Each is in the
/// coordinates after the moves before it (a parent's before its children's),
/// so they apply in order.
type Move = (String, Option<String>);

fn translate(schema: &mut Value, dialect: Dialect, base: &Url, pointer: &str, moves: &mut Vec<Move>) -> Result<(), Untranslatable> {
  let Value::Object(object) = schema else {
    return Ok(());
  };
  // a `$schema` within switches the dialect for what's under it (the
  // root's was checked, and is the caller's `dialect`)
  let dialect = match object.get("$schema") {
    Some(Value::String(uri)) if !pointer.is_empty() => Dialect::of(uri).ok_or_else(|| Untranslatable::UnknownDialect {
      dialect: uri.clone(),
      pointer: pointer.to_string(),
    })?,
    Some(other) if !pointer.is_empty() => {
      return Err(Untranslatable::UnknownDialect {
        dialect: other.to_string(),
        pointer: pointer.to_string(),
      });
    }
    _ => dialect,
  };
  object.shift_remove("$schema");
  // what the draft ignores, 2020-12 would apply: a keyword the draft doesn't
  // know, and what's next to a `$ref` in the drafts before 2019-09
  let unknown = object.keys().filter(|keyword| !dialect.knows(keyword)).cloned().collect::<Vec<_>>();
  for keyword in unknown {
    object.shift_remove(&keyword);
    moves.push((pointer::append(pointer, &keyword), None));
  }
  if dialect.ignores_next_to_ref() && object.contains_key("$ref") {
    for keyword in IGNORED_NEXT_TO_REF {
      if object.shift_remove(*keyword).is_some() {
        moves.push((pointer::append(pointer, keyword), None));
      }
    }
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
      moves.push((format!("{}/items", pointer), Some(format!("{}/prefixItems", pointer))));
      if let Some(additional) = object.shift_remove("additionalItems") {
        object.insert("items".to_string(), additional);
        moves.push((format!("{}/additionalItems", pointer), Some(format!("{}/items", pointer))));
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
      if let serde_json::map::Entry::Vacant(entry) = entries.entry(name.clone()) {
        entry.insert(dependency);
        moves.push((
          pointer::append(&format!("{}/dependencies", pointer), &name),
          Some(pointer::append(&format!("{}/{}", pointer, keyword), &name)),
        ));
      }
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
            translate(schema, dialect, &base, &pointer::append(&keyword_pointer, name), moves)?;
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
            translate(schema, dialect, &base, &format!("{}/{}", keyword_pointer, index), moves)?;
          }
        }
        value => translate(value, dialect, &base, &keyword_pointer, moves)?,
      },
      // data (ex. `default`), or a keyword dprint doesn't know, which an
      // extension may use for anything (ex. an `x-tool` with a `$ref`)
      _ => {}
    }
  }
  Ok(())
}

/// Rewrites each `$ref` by JSON pointer into a schema the translation moved
/// to where that schema is now. A reference is resolved the way a validator
/// resolves it (against the `$id` of the resource it's in), its pointer
/// taken through the moves in order, and written back in the form it had: a
/// fragment of its resource, or a uri. One into what the translation
/// dropped can't be kept, which refuses the schema.
fn rewrite_references_into_moved_schemas(schema: &mut Value, uri: &Url, moves: &[Move]) -> Result<(), Untranslatable> {
  let index = ResourceIndex::of(schema, uri);
  let mut rewrites = Vec::new();
  let mut refused = None;
  walk(schema, &mut |object, pointer| {
    if refused.is_some() {
      return;
    }
    let Some(Value::String(reference)) = object.get("$ref") else {
      return;
    };
    let Some((resource_uri, resource_pointer, fragment)) = index.resolve_pointer(reference, pointer) else {
      return;
    };
    let before = format!("{}{}", resource_pointer, fragment);
    let mut after = before.clone();
    for (from, to) in moves {
      if after == *from || after.starts_with(&format!("{}/", from)) {
        match to {
          Some(to) => after = format!("{}{}", to, &after[from.len()..]),
          None => {
            refused = Some(Untranslatable::ReferenceIntoDropped {
              reference: reference.clone(),
              pointer: pointer.to_string(),
              dropped: from.clone(),
            });
            return;
          }
        }
      }
    }
    if after != before {
      let fragment = &after[resource_pointer.len()..];
      let rewritten = if reference.starts_with('#') {
        pointer::fragment_reference(fragment)
      } else {
        pointer::resource_reference(resource_uri.as_str(), fragment)
      };
      rewrites.push((pointer.to_string(), rewritten));
    }
  });
  if let Some(refused) = refused {
    return Err(refused);
  }
  for (pointer, reference) in rewrites {
    if let Some(Value::Object(object)) = schema.pointer_mut(&pointer) {
      object.insert("$ref".to_string(), Value::String(reference));
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

  /// What a `$ref` at `pointer` by JSON pointer refers to: the uri of the
  /// resource, the resource's JSON pointer in the document, and the pointer
  /// within the resource. `None` for a reference to another document, to a
  /// resource as a whole or to an anchor.
  fn resolve_pointer(&self, reference: &str, pointer: &str) -> Option<(Url, &str, String)> {
    let target = self.base_of(pointer).join(reference).ok()?;
    let fragment = target.fragment().map(pointer::decode_fragment).unwrap_or_default();
    if !fragment.starts_with('/') {
      return None;
    }
    let mut resource = target;
    resource.set_fragment(None);
    let resource_pointer = self.resources.get(&resource)?;
    Some((resource.clone(), resource_pointer.as_str(), fragment))
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
      Err(Untranslatable::UnknownDialect {
        dialect: "https://example.com/my-dialect".to_string(),
        pointer: "/definitions/custom".to_string(),
      })
    );
    // nothing of it was touched
    assert_eq!(schema["definitions"]["custom"], custom);
    // not even for a `$schema` that isn't a string
    let mut schema = json!({ "properties": { "a": { "$schema": 7 } } });
    assert_eq!(
      to_2020_12(&mut schema, Dialect::Draft07, &uri()),
      Err(Untranslatable::UnknownDialect {
        dialect: "7".to_string(),
        pointer: "/properties/a".to_string(),
      })
    );
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
        // (not a draft-07 keyword, so ignored by it and dropped)
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
    assert_eq!(schema["dependentRequired"], json!({ "a": ["b"] }));
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
  fn drops_what_the_draft_ignores_and_2020_12_would_apply() {
    let schema = translated(
      json!({
        "$schema": "http://json-schema.org/draft-04/schema#",
        "$id": "https://example.com/not-a-draft-04-keyword.json",
        "if": {},
        "then": { "not": {} },
        "else": {},
        "const": 1,
        "contains": {},
        "propertyNames": {},
        "$anchor": "x",
        "unevaluatedProperties": false,
        "prefixItems": [{}],
        "dependentSchemas": {},
        "description": "kept",
        "format": "kept",
        "x-tool": "kept",
        "properties": { "a": { "if": {}, "then": {} } },
      }),
      Dialect::Draft04,
    );
    assert_eq!(
      schema,
      json!({ "$id": uri().as_str(), "description": "kept", "format": "kept", "x-tool": "kept", "properties": { "a": {} } })
    );
    // a draft knows what came with it or before it
    let schema = translated(
      json!({ "if": {}, "then": {}, "const": 1, "$anchor": "x", "unevaluatedProperties": false, "prefixItems": [{}] }),
      Dialect::Draft07,
    );
    assert_eq!(schema, json!({ "$id": uri().as_str(), "if": {}, "then": {}, "const": 1 }));
    let schema = translated(
      json!({ "unevaluatedProperties": false, "prefixItems": [{}], "$dynamicRef": "#meta" }),
      Dialect::Draft2019_09,
    );
    assert_eq!(schema, json!({ "$id": uri().as_str(), "unevaluatedProperties": false }));
    let schema = translated(json!({ "prefixItems": [{}], "$dynamicRef": "#meta" }), Dialect::Draft2020_12);
    assert_eq!(schema, json!({ "$id": uri().as_str(), "prefixItems": [{}], "$dynamicRef": "#meta" }));
  }

  #[test]
  fn refuses_a_reference_into_what_it_drops() {
    let mut schema = json!({
      "$schema": "http://json-schema.org/draft-04/schema#",
      "definitions": { "x": { "if": { "properties": { "b": { "type": "string" } } } } },
      "properties": { "a": { "$ref": "#/definitions/x/if/properties/b" } },
    });
    assert_eq!(
      to_2020_12(&mut schema, Dialect::Draft04, &uri()),
      Err(Untranslatable::ReferenceIntoDropped {
        reference: "#/definitions/x/if/properties/b".to_string(),
        pointer: "/properties/a".to_string(),
        dropped: "/definitions/x/if".to_string(),
      })
    );
    // what a draft before 2019-09 ignores next to a `$ref` is dropped too
    let mut schema = json!({
      "definitions": { "x": { "$ref": "#/definitions/y", "properties": { "b": {} } }, "y": {} },
      "properties": { "a": { "$ref": "#/definitions/x/properties/b" } },
    });
    assert_eq!(
      to_2020_12(&mut schema, Dialect::Draft07, &uri()),
      Err(Untranslatable::ReferenceIntoDropped {
        reference: "#/definitions/x/properties/b".to_string(),
        pointer: "/properties/a".to_string(),
        dropped: "/definitions/x/properties".to_string(),
      })
    );
    // while a reference to what stays is fine
    let mut schema = json!({
      "definitions": { "x": { "$ref": "#/definitions/y", "description": "kept" }, "y": {} },
      "properties": { "a": { "$ref": "#/definitions/x/description" } },
    });
    assert_eq!(to_2020_12(&mut schema, Dialect::Draft07, &uri()), Ok(()));
  }

  #[test]
  fn rewrites_references_into_what_it_moved() {
    let schema = translated(
      json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "definitions": {
          "pair": { "type": "array", "items": [{ "type": "string" }, { "type": "integer" }], "additionalItems": false },
          "nested": { "items": [{ "items": [{ "type": "boolean" }], "additionalItems": { "type": "null" } }] },
          "inner": {
            "$id": "inner/schema.json",
            "definitions": { "t": { "items": [{ "type": "string" }] } },
            "properties": { "p": { "$ref": "#/definitions/t/items/0" } },
          },
        },
        "properties": {
          "label": { "$ref": "#/definitions/pair/items/0" },
          "count": { "$ref": "https://plugins.dprint.dev/test/schema.json#/definitions/pair/items/1" },
          "rest": { "$ref": "#/definitions/pair/additionalItems" },
          "deep": { "$ref": "#/definitions/nested/items/0/items/0" },
          "deeper": { "$ref": "#/definitions/nested/items/0/additionalItems" },
          "dependent": { "$ref": "#/dependencies/a/properties/b" },
          "pair": { "$ref": "#/definitions/pair" },
          "into_inner": { "$ref": "inner/schema.json#/definitions/t/items/0" },
          "elsewhere": { "$ref": "https://example.com/other.json#/definitions/pair/items/0" },
        },
        "dependencies": { "a": { "properties": { "b": { "type": "number" } } } },
      }),
      Dialect::Draft07,
    );
    let reference = |name: &str| schema["properties"][name]["$ref"].as_str().unwrap().to_string();
    assert_eq!(reference("label"), "#/definitions/pair/prefixItems/0");
    assert_eq!(
      reference("count"),
      "https://plugins.dprint.dev/test/schema.json#/definitions/pair/prefixItems/1"
    );
    assert_eq!(reference("rest"), "#/definitions/pair/items");
    assert_eq!(reference("deep"), "#/definitions/nested/prefixItems/0/prefixItems/0");
    assert_eq!(reference("deeper"), "#/definitions/nested/prefixItems/0/items");
    assert_eq!(reference("dependent"), "#/dependentSchemas/a/properties/b");
    // what wasn't moved, or is in another document, stays as it is
    assert_eq!(reference("pair"), "#/definitions/pair");
    assert_eq!(reference("elsewhere"), "https://example.com/other.json#/definitions/pair/items/0");
    // a reference within a nested resource is relative to it, and one into
    // it from outside is written as its uri
    assert_eq!(
      schema["definitions"]["inner"]["properties"]["p"]["$ref"],
      json!("#/definitions/t/prefixItems/0")
    );
    assert_eq!(
      reference("into_inner"),
      "https://plugins.dprint.dev/test/inner/schema.json#/definitions/t/prefixItems/0"
    );
    // each refers to the schema it did
    let index = ResourceIndex::of(&schema, &uri());
    let target = |reference: &str, from: &str| schema.pointer(&index.resolve(reference, from).unwrap()).cloned().unwrap();
    assert_eq!(target(&reference("label"), "/properties/label"), json!({ "type": "string" }));
    assert_eq!(target(&reference("count"), "/properties/count"), json!({ "type": "integer" }));
    assert_eq!(target(&reference("rest"), "/properties/rest"), json!(false));
    assert_eq!(target(&reference("deep"), "/properties/deep"), json!({ "type": "boolean" }));
    assert_eq!(target(&reference("deeper"), "/properties/deeper"), json!({ "type": "null" }));
    assert_eq!(target(&reference("dependent"), "/properties/dependent"), json!({ "type": "number" }));
    assert_eq!(target(&reference("into_inner"), "/properties/into_inner"), json!({ "type": "string" }));
    assert_eq!(
      target("#/definitions/t/prefixItems/0", "/definitions/inner/properties/p"),
      json!({ "type": "string" })
    );
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
