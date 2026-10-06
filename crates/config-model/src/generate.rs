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

/// Makes a generated schema say what a configuration file may hold rather
/// than what the Rust types do: a property of an `Option` may be left out,
/// but can't be `null`, and an integer's width is Rust's, not the file's.
#[derive(Clone, Debug)]
struct ConfigurationValues;

impl Transform for ConfigurationValues {
  fn transform(&mut self, schema: &mut Schema) {
    if let Some(object) = schema.as_object_mut() {
      if let Some(Value::Array(types)) = object.get_mut("type") {
        types.retain(|kind| kind != "null");
        if types.len() == 1 {
          let kind = types.pop().unwrap();
          object.insert("type".to_string(), kind);
        }
      }
      if let Some(Value::Array(branches)) = object.get_mut("anyOf") {
        branches.retain(|branch| branch != &serde_json::json!({ "type": "null" }));
        if branches.len() == 1
          && let Value::Object(branch) = branches.pop().unwrap()
        {
          object.shift_remove("anyOf");
          for (key, value) in branch {
            object.entry(key).or_insert(value);
          }
        }
      }
      if object
        .get("format")
        .and_then(Value::as_str)
        .is_some_and(|format| format.starts_with("int") || format.starts_with("uint"))
      {
        object.shift_remove("format");
      }
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
  fn describes_a_configuration_file_rather_than_the_rust_types() {
    let schema = schema_for::<Example>();
    assert_eq!(schema.as_object().unwrap().keys().take(2).collect::<Vec<_>>(), vec!["$schema", "title"]);
    assert_eq!(schema["$schema"], json!("https://json-schema.org/draft/2020-12/schema"));
    assert_eq!(schema["properties"]["flag"], json!({ "description": "May be left out.", "type": "boolean" }));
    assert_eq!(schema["properties"]["count"], json!({ "type": "integer", "minimum": 0, "maximum": 255 }));
    assert_eq!(
      schema["properties"]["which"],
      json!({
        "description": "One or more.",
        "anyOf": [{ "type": "string" }, { "type": "array", "items": { "type": "string" } }]
      })
    );
    assert_eq!(schema["properties"]["name"], json!({ "type": "string" }));
    assert_eq!(schema["required"], json!(["name"]));
  }
}
