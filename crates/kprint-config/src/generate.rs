use schemars::JsonSchema;
use schemars::Schema;
use schemars::generate::SchemaSettings;
use schemars::transform::Transform;
use schemars::transform::transform_subschemas;
use serde_json::Value;

/// The JSON schema (2020-12) of a configuration type, with `$schema` first.
pub fn schema_for<T: JsonSchema>() -> Value {
  let schema = SchemaSettings::draft2020_12()
    .with_transform(ConfigurationValues)
    .into_generator()
    .into_root_schema_for::<T>();
  let mut root = serde_json::Map::new();
  let Value::Object(generated) = schema.to_value() else {
    unreachable!("a root schema is an object");
  };
  // the dialect first, then what names the schema
  const FIRST: [&str; 4] = ["$schema", "$id", "title", "description"];
  for key in FIRST {
    if let Some(value) = generated.get(key) {
      root.insert(key.to_string(), value.clone());
    }
  }
  root.extend(generated.into_iter().filter(|(key, _)| !FIRST.contains(&key.as_str())));
  Value::Object(root)
}

/// [`schema_for`], as pretty-printed JSON with a final newline.
pub fn schema_json_for<T: JsonSchema>() -> String {
  format!("{}\n", serde_json::to_string_pretty(&schema_for::<T>()).expect("a schema serializes"))
}

/// Leaves out of a generated schema what is about the Rust types rather than
/// a configuration file: an integer's width (schemars' `format`, ex.
/// `uint8`), which isn't a JSON schema format. That's an annotation; what the
/// schema accepts is what the types do, including `null` for a property of an
/// `Option`, which reads as the property left out.
#[derive(Clone, Debug)]
struct ConfigurationValues;

impl Transform for ConfigurationValues {
  fn transform(&mut self, schema: &mut Schema) {
    if let Some(object) = schema.as_object_mut()
      && object
        .get("format")
        .and_then(Value::as_str)
        .is_some_and(|format| format.starts_with("int") || format.starts_with("uint"))
    {
      object.shift_remove("format");
    }
    transform_subschemas(self, schema);
  }
}

#[cfg(test)]
mod test {
  use pretty_assertions::assert_eq;
  use serde_json::json;

  use super::*;

  #[derive(JsonSchema)]
  #[allow(dead_code)]
  struct Example {
    /// May be left out.
    flag: Option<bool>,
    count: Option<u8>,
    /// One or more.
    which: Option<Which>,
    name: String,
  }

  #[derive(JsonSchema)]
  #[serde(untagged)]
  #[schemars(inline)]
  #[allow(dead_code)]
  enum Which {
    One(String),
    Many(Vec<String>),
  }

  #[test]
  fn describes_what_the_types_accept_without_their_rust_widths() {
    let schema = schema_for::<Example>();
    assert_eq!(schema.as_object().unwrap().keys().take(2).collect::<Vec<_>>(), vec!["$schema", "title"]);
    assert_eq!(schema["$schema"], json!("https://json-schema.org/draft/2020-12/schema"));
    // an `Option` reads `null` as left out, so the schema allows it
    assert_eq!(
      schema["properties"]["flag"],
      json!({ "description": "May be left out.", "type": ["boolean", "null"] })
    );
    assert_eq!(
      schema["properties"]["count"],
      json!({ "type": ["integer", "null"], "minimum": 0, "maximum": 255 })
    );
    assert_eq!(
      schema["properties"]["which"],
      json!({
        "description": "One or more.",
        "anyOf": [{ "anyOf": [{ "type": "string" }, { "type": "array", "items": { "type": "string" } }] }, { "type": "null" }]
      })
    );
    assert_eq!(schema["properties"]["name"], json!({ "type": "string" }));
    assert_eq!(schema["required"], json!(["name"]));
  }
}
