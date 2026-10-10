//! A configuration schema as a document to look things up in, for a tool
//! that reads it (ex. the language server's completions).

use std::collections::HashMap;

use serde_json::Value;
use url::Url;

use crate::pointer;
use crate::translate::walk;

/// A configuration schema (see [`crate::build_config_schema`]) with its
/// references resolvable the way a validator does: a `$ref` is resolved
/// against the `$id` of the resource it's in, and a resource embedded under
/// `$defs` is found by its `$id`.
pub struct SchemaDocument {
  root: Value,
  /// By uri, the JSON pointer of each resource (a schema with an `$id`).
  resources: HashMap<Url, String>,
  /// By pointer, the uri of each resource.
  bases: Vec<(String, Url)>,
  /// By the uri of the resource it's in (none for the document itself)
  /// and name, the JSON pointer of each anchor.
  anchors: HashMap<(Option<Url>, String), String>,
}

impl SchemaDocument {
  pub fn new(root: Value) -> Self {
    let mut document = Self {
      root,
      resources: HashMap::new(),
      bases: Vec::new(),
      anchors: HashMap::new(),
    };
    let mut found = Vec::new();
    walk(&document.root, &mut |object, pointer| {
      found.push((
        pointer.to_string(),
        object.get("$id").cloned(),
        object.get("$anchor").cloned(),
        object.get("$dynamicAnchor").cloned(),
      ));
    });
    for (pointer, id, anchor, dynamic_anchor) in found {
      if let Some(Value::String(id)) = id
        && let Ok(id) = Url::parse(&id)
      {
        document.resources.insert(id.clone(), pointer.clone());
        document.bases.push((pointer.clone(), id));
      }
      for name in [anchor, dynamic_anchor].into_iter().flatten() {
        if let Value::String(name) = name {
          let base = document.base_of(&pointer).cloned();
          document.anchors.insert((base, name), pointer.clone());
        }
      }
    }
    document
  }

  pub fn root(&self) -> &Value {
    &self.root
  }

  /// The schema at `pointer`.
  pub fn node(&self, pointer: &str) -> Option<&Value> {
    self.root.pointer(pointer)
  }

  /// The uri of the resource the schema at `pointer` is in, if it's in one
  /// (the document itself has no uri).
  fn base_of(&self, pointer: &str) -> Option<&Url> {
    self
      .bases
      .iter()
      .filter(|(resource, _)| pointer == resource || pointer.starts_with(&format!("{}/", resource)))
      .max_by_key(|(resource, _)| resource.len())
      .map(|(_, uri)| uri)
  }

  /// The JSON pointer of what a `$ref` at `pointer` refers to, when it's
  /// in the document.
  pub fn resolve(&self, reference: &str, pointer: &str) -> Option<String> {
    let target = match self.base_of(pointer) {
      Some(base) => base.join(reference).ok()?,
      None => match Url::parse(reference) {
        Ok(target) => target,
        // the document has no uri, so only a fragment refers into it
        Err(_) => {
          let fragment = pointer::decode_fragment(reference.strip_prefix('#')?);
          return if fragment.is_empty() || fragment.starts_with('/') {
            Some(fragment)
          } else {
            self.anchors.get(&(None, fragment)).cloned()
          };
        }
      },
    };
    let fragment = target.fragment().map(pointer::decode_fragment).unwrap_or_default();
    let mut resource = target;
    resource.set_fragment(None);
    let resource_pointer = self.resources.get(&resource)?;
    if fragment.is_empty() || fragment.starts_with('/') {
      return Some(format!("{}{}", resource_pointer, fragment));
    }
    self.anchors.get(&(Some(resource), fragment)).cloned()
  }
}

#[cfg(test)]
mod test {
  use pretty_assertions::assert_eq;
  use serde_json::json;

  use super::*;

  #[test]
  fn resolves_references_within_the_document_and_its_resources() {
    let document = SchemaDocument::new(json!({
      "properties": {
        "test": { "allOf": [{ "$ref": "https://plugins.dprint.dev/test/schema.json" }] },
        "named": { "$ref": "#root-anchor" },
      },
      "$defs": {
        "pluginTable": { "$anchor": "root-anchor", "type": "object" },
        "plugin:test": {
          "$id": "https://plugins.dprint.dev/test/schema.json",
          "properties": { "a": { "$ref": "#/definitions/a" }, "b": { "$ref": "#kleur" }, "c": { "$ref": "inner/schema.json#/definitions/y" } },
          "definitions": {
            "a": { "type": "string" },
            "color": { "$anchor": "kleur" },
            "inner": { "$id": "https://plugins.dprint.dev/test/inner/schema.json", "definitions": { "y": { "$anchor": "y" } } },
          },
        },
      },
    }));
    assert_eq!(document.resolve("#/$defs/pluginTable", ""), Some("/$defs/pluginTable".to_string()));
    assert_eq!(document.resolve("#root-anchor", "/properties/named"), Some("/$defs/pluginTable".to_string()));
    assert_eq!(
      document.resolve("https://plugins.dprint.dev/test/schema.json", "/properties/test/allOf/0"),
      Some("/$defs/plugin:test".to_string())
    );
    assert_eq!(
      document.resolve("#/definitions/a", "/$defs/plugin:test/properties/a"),
      Some("/$defs/plugin:test/definitions/a".to_string())
    );
    assert_eq!(
      document.resolve("#kleur", "/$defs/plugin:test/properties/b"),
      Some("/$defs/plugin:test/definitions/color".to_string())
    );
    assert_eq!(
      document.resolve("inner/schema.json#/definitions/y", "/$defs/plugin:test/properties/c"),
      Some("/$defs/plugin:test/definitions/inner/definitions/y".to_string())
    );
    assert_eq!(
      document.resolve("#y", "/$defs/plugin:test/definitions/inner/properties/z"),
      Some("/$defs/plugin:test/definitions/inner/definitions/y".to_string())
    );
    assert_eq!(document.resolve("https://example.com/other.json", ""), None);
    assert_eq!(document.resolve("other.json", ""), None);
    assert_eq!(document.resolve("#nowhere", ""), None);
  }
}
