//! Reading configuration values into the model, and the model back into
//! values.
//!
//! A configuration file is parsed into dprint-core's format-neutral
//! [`ConfigKeyMap`] (JSON with comments and TOML read into the same values),
//! and a plugin gets its configuration as one. Reading those as a type of the
//! model goes through serde, with the path of whatever is wrong.

use std::fmt;

use dprint_core::configuration::ConfigKeyMap;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde::de::Error as _;

/// Why configuration values aren't what a type says they may be: what's
/// wrong, and the path of the value it's about (ex.
/// `typescript.overrides[0].files`), which is empty for the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueError {
  pub path: String,
  pub message: String,
}

impl ValueError {
  fn new(path: &serde_path_to_error::Path, message: impl fmt::Display) -> Self {
    ValueError {
      path: if path.iter().next().is_none() { String::new() } else { path.to_string() },
      message: message.to_string(),
    }
  }
}

impl fmt::Display for ValueError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    if self.path.is_empty() {
      f.write_str(&self.message)
    } else {
      write!(f, "{}: {}", self.path, self.message)
    }
  }
}

impl std::error::Error for ValueError {}

/// Reads configuration values (a configuration file's, or a plugin's) as a
/// type of the model.
pub fn from_values<T: DeserializeOwned>(values: ConfigKeyMap) -> Result<T, ValueError> {
  let value = serde_json::to_value(values).map_err(|err| ValueError {
    path: String::new(),
    message: err.to_string(),
  })?;
  from_json(value)
}

/// Reads a JSON value as a type of the model.
pub fn from_json<T: DeserializeOwned>(value: serde_json::Value) -> Result<T, ValueError> {
  serde_path_to_error::deserialize(value).map_err(|err| ValueError::new(err.path(), err.inner()))
}

/// The configuration values of a type of the model (an object), ex. to hand
/// to dprint-core's `resolve_global_config` or to a plugin.
pub fn to_values<T: Serialize>(value: &T) -> Result<ConfigKeyMap, ValueError> {
  let value = serde_json::to_value(value).map_err(|err| ValueError {
    path: String::new(),
    message: err.to_string(),
  })?;
  from_json(value)
}

/// Reads a flattened part of a struct, with the path of an error in it.
///
/// serde reads a `#[serde(flatten)]` field from what the struct's other
/// fields didn't take, which the path tracking of [`from_values`] doesn't
/// see into: an error in it would say what's wrong but not where. So the
/// part is tracked here, and the path goes in front of the message.
pub(crate) fn tracked<'de, T: Deserialize<'de>, D: Deserializer<'de>>(deserializer: D) -> Result<T, D::Error> {
  serde_path_to_error::deserialize(deserializer).map_err(|err| D::Error::custom(ValueError::new(err.path(), err.inner())))
}

#[cfg(test)]
mod test {
  use dprint_core::configuration::ConfigKeyValue;
  use pretty_assertions::assert_eq;
  use serde::Deserialize;
  use serde::Serialize;

  use super::*;

  #[derive(Debug, PartialEq, Serialize, Deserialize)]
  #[serde(rename_all = "camelCase")]
  struct Settings {
    line_width: Option<u32>,
    names: Vec<String>,
  }

  #[test]
  fn reads_values_as_a_type_and_the_type_as_values() {
    let values = ConfigKeyMap::from([
      ("lineWidth".to_string(), ConfigKeyValue::from_i32(80)),
      ("names".to_string(), ConfigKeyValue::Array(vec![ConfigKeyValue::from_str("a")])),
    ]);
    let settings: Settings = from_values(values.clone()).unwrap();
    assert_eq!(
      settings,
      Settings {
        line_width: Some(80),
        names: vec!["a".to_string()]
      }
    );
    assert_eq!(to_values(&settings).unwrap(), values);
  }

  #[test]
  fn says_where_a_value_is_wrong() {
    let values = ConfigKeyMap::from([
      ("lineWidth".to_string(), ConfigKeyValue::from_i32(80)),
      (
        "names".to_string(),
        ConfigKeyValue::Array(vec![ConfigKeyValue::from_str("a"), ConfigKeyValue::from_i32(1)]),
      ),
    ]);
    let err = from_values::<Settings>(values).unwrap_err();
    assert_eq!(err.path, "names[1]");
    assert_eq!(err.message, "invalid type: integer `1`, expected a string");
    assert_eq!(err.to_string(), "names[1]: invalid type: integer `1`, expected a string");

    let err = from_values::<Settings>(ConfigKeyMap::new()).unwrap_err();
    assert_eq!(err.path, "");
    assert_eq!(err.to_string(), "missing field `names`");
  }
}
