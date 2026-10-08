//! One schema for a configuration file: dprint's, with each plugin's
//! schema for its table.

use std::collections::HashMap;
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
use crate::translate::Untranslatable;
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
/// A plugin schema that can't be made a 2020-12 resource (of a draft dprint
/// doesn't know, or with a reference into what its draft ignores, see
/// [`Untranslatable`]) is referred to by its url.
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
      Err(why) => {
        let why = match why {
          Untranslatable::UnknownDialect { dialect, pointer } => {
            let at = if pointer.is_empty() { String::new() } else { format!(" (at {})", pointer) };
            format!("is for {}{}, a JSON schema draft dprint doesn't know", dialect, at)
          }
          Untranslatable::ReferenceIntoDropped { reference, pointer, dropped } => format!(
            "refers to {} (at {}) into what its JSON schema draft ignores ({}) and 2020-12 would apply, which can't be translated",
            reference, pointer, dropped
          ),
        };
        match &plugin.url {
          Some(url) => {
            warnings.push(format!(
              concat!(
                "The configuration schema of the {} {}, so it's referred to by its url instead. ",
                "Editors may report dprint's own properties of its table (ex. `associations`) as unknown."
              ),
              plugin_display, why
            ));
            object_entry(&mut root, "properties").insert(
              plugin.config_key,
              serde_json::json!({ "description": description, "type": "object", "properties": table_properties, "allOf": [{ "$ref": url.as_str() }] }),
            );
          }
          None => warnings.push(format!(
            "The configuration schema of the {} plugin {}, so it's left out.",
            plugin.config_key, why
          )),
        }
        continue;
      }
    };
    let table_schemas = TableSchemas::of(&resource);
    if let Some((pointer, keyword)) = &table_schemas.whole_object {
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
    if let Some(reference) = &table_schemas.not_followed {
      warnings.push(format!(
        "The configuration schema of the {} refers to {} for its table, so editors may report dprint's own properties of its table (ex. `associations`) as unknown.",
        plugin_display, reference
      ));
    }
    // the schema of its overrides goes in the plugin's resource, as what a
    // resource refers to has to be within it (or at an absolute uri)
    let override_name = override_definition_name(&resource.schema);
    let override_reference = pointer::resource_reference(resource.uri.as_str(), &pointer::append("/$defs", &override_name));
    let override_schema = override_schema(&resource, &table_schemas, &table_properties, &files);
    if let Some((pointer, keyword)) = &table_schemas.conditional_properties {
      warnings.push(format!(
        concat!(
          "The configuration schema of the {} says which properties its table has on a condition too (`{}`{}), which an override ",
          "can't be checked on by itself, as the table it's merged into decides it, so editors check an override's properties only as far ",
          "as the schema says unconditionally."
        ),
        plugin_display,
        keyword,
        if pointer.is_empty() { String::new() } else { format!(" at {}", pointer) }
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
  let mut schema = Value::Object(result);
  let mut options = json_schema_sort::SortOptions::default();
  options.properties = json_schema_sort::PropertyOrdering::Preserve;
  json_schema_sort::sort_schema_with_options(&mut schema, options);
  Ok(ConfigSchema { schema, warnings })
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
  fn of(schema: Value, url: Option<&Url>, config_key: &str) -> Result<Self, Untranslatable> {
    let dialect = match schema.get("$schema") {
      None => Dialect::DEFAULT,
      Some(Value::String(uri)) => Dialect::of(uri).ok_or_else(|| Untranslatable::UnknownDialect {
        dialect: uri.clone(),
        pointer: String::new(),
      })?,
      Some(other) => {
        return Err(Untranslatable::UnknownDialect {
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
///
/// Where a schema is isn't how it applies: the same schema can be applied by
/// several schemas (two `$ref`s to it), and unconditionally by one and on a
/// condition by another. What's collected here is about the applications,
/// so each one counts, however many times the schema is reached.
struct TableSchemas {
  /// Where the first keyword about the table as a whole is, and which.
  whole_object: Option<(String, &'static str)>,
  /// The schemas that apply to the table unconditionally (the plugin's
  /// schema, its `$ref` targets and `allOf` items, in turn), whose
  /// properties every override may set.
  unconditional: Vec<String>,
  /// By each schema, the ones it applies unconditionally (its `$ref` target
  /// and `allOf` items): what its `unevaluatedProperties` sees, together
  /// with its own properties.
  applied: HashMap<String, Vec<String>>,
  /// The schemas in `unconditional` that apply, on a condition (ex. in an
  /// `anyOf` branch), a schema that evaluates properties (`properties`, or
  /// an `additionalProperties` or `unevaluatedProperties`, whatever it
  /// says): what their `unevaluatedProperties` has left depends on the
  /// table.
  evaluating_on_a_condition: HashSet<String>,
  /// Where the first schema that applies to the table on a condition (ex.
  /// an `anyOf` branch) says which properties it has (what they may be,
  /// or that it has one), and the keyword it's under. An override can't be
  /// checked against it on its own, as the table it's merged into decides
  /// the condition, and the properties it adds can change that.
  conditional_properties: Option<(String, &'static str)>,
  /// A reference to another document that couldn't be followed.
  not_followed: Option<String>,
  /// The schemas with a reference that isn't followed (to another
  /// document, or a `$dynamicRef`): what they apply isn't known, so what
  /// their `unevaluatedProperties` has left isn't either.
  unfollowed: HashSet<String>,
}

/// What a schema applies to the instance it applies to.
struct Applies {
  /// Unconditionally: its `$ref` target and its `allOf` items.
  always: Vec<String>,
  /// On a condition the instance decides: the branches of its `anyOf`,
  /// `oneOf`, `if`/`then`/`else` and `not`, and its `dependentSchemas`,
  /// each with the keyword.
  on_a_condition: Vec<(String, &'static str)>,
  /// A `$ref` that couldn't be followed.
  not_followed: Option<String>,
}

impl Applies {
  fn of(resource: &Resource, pointer: &str, object: &Map<String, Value>) -> Self {
    let mut result = Self {
      always: Vec::new(),
      on_a_condition: Vec::new(),
      not_followed: None,
    };
    // (what's next to a `$ref` applies too, in 2020-12)
    if let Some(reference) = object.get("$ref") {
      match reference.as_str().and_then(|reference| resource.index.resolve(reference, pointer)) {
        Some(target) => result.always.push(target),
        None => result.not_followed = Some(reference.as_str().map(ToOwned::to_owned).unwrap_or_else(|| reference.to_string())),
      }
    }
    // (a `$dynamicRef` is resolved by what applied the schema, which the
    // table decides, so what it refers to isn't followed)
    if let Some(reference) = object.get("$dynamicRef") {
      result.not_followed = Some(reference.as_str().map(ToOwned::to_owned).unwrap_or_else(|| reference.to_string()));
    }
    if let Some(Value::Array(items)) = object.get("allOf") {
      result.always.extend((0..items.len()).map(|index| format!("{}/allOf/{}", pointer, index)));
    }
    for keyword in ["anyOf", "oneOf"] {
      if let Some(Value::Array(branches)) = object.get(keyword) {
        result
          .on_a_condition
          .extend((0..branches.len()).map(|index| (format!("{}/{}/{}", pointer, keyword, index), keyword)));
      }
    }
    // (`then` and `else` are ignored without an `if`)
    let conditionals: &[&'static str] = if object.contains_key("if") {
      &["if", "then", "else", "not"]
    } else {
      &["not"]
    };
    for keyword in conditionals {
      if object.contains_key(*keyword) {
        result.on_a_condition.push((format!("{}/{}", pointer, keyword), keyword));
      }
    }
    if let Some(Value::Object(dependencies)) = object.get("dependentSchemas") {
      for name in dependencies.keys() {
        result
          .on_a_condition
          .push((pointer::append(&format!("{}/dependentSchemas", pointer), name), "dependentSchemas"));
      }
    }
    result
  }
}

impl TableSchemas {
  fn of(resource: &Resource) -> Self {
    const WHOLE_OBJECT_KEYWORDS: &[&str] = &["minProperties", "maxProperties", "propertyNames", "const", "enum", "patternProperties"];
    const EVALUATES_PROPERTIES: &[&str] = &["properties", "additionalProperties", "unevaluatedProperties"];
    let mut result = Self {
      whole_object: None,
      unconditional: Vec::new(),
      applied: HashMap::new(),
      evaluating_on_a_condition: HashSet::new(),
      conditional_properties: None,
      not_followed: None,
      unfollowed: HashSet::new(),
    };
    let schema_at = |pointer: &str| resource.schema.pointer(pointer).and_then(Value::as_object);
    let whole_object_keyword = |object: &Map<String, Value>| WHOLE_OBJECT_KEYWORDS.iter().copied().find(|keyword| object.contains_key(*keyword));
    // what applies unconditionally: the plugin's schema and what it applies
    // unconditionally, in turn. Each schema is looked at once, and what it
    // applies is recorded then, so a schema applied by two is applied by
    // both. Nearest first, so a warning names the first of what's at a
    // level.
    let mut pending = VecDeque::from([String::new()]);
    let mut visited = HashSet::new();
    // what's applied on a condition: where, under which keyword, by which
    // unconditional schema, and whether what it evaluates counts (not
    // under a `not`, which evaluates nothing)
    let mut on_a_condition = VecDeque::new();
    while let Some(pointer) = pending.pop_front() {
      if !visited.insert(pointer.clone()) {
        continue;
      }
      let Some(object) = schema_at(&pointer) else {
        continue;
      };
      result.unconditional.push(pointer.clone());
      if result.whole_object.is_none()
        && let Some(keyword) = whole_object_keyword(object)
      {
        result.whole_object = Some((pointer.clone(), keyword));
      }
      // a dependency is a condition of its own: the property an override
      // adds can be the one that requires others
      if object.contains_key("dependentRequired") && result.conditional_properties.is_none() {
        result.conditional_properties = Some((pointer.clone(), "dependentRequired"));
      }
      let applies = Applies::of(resource, &pointer, object);
      if let Some(reference) = applies.not_followed {
        result.not_followed.get_or_insert(reference);
        result.unfollowed.insert(pointer.clone());
      }
      on_a_condition.extend(
        applies
          .on_a_condition
          .into_iter()
          .map(|(target, keyword)| (target, keyword, pointer.clone(), keyword != "not")),
      );
      pending.extend(applies.always.iter().cloned());
      result.applied.insert(pointer, applies.always);
    }
    // the plugin's schema first
    result.unconditional.sort();
    // what applies on a condition, and everything it applies in turn (the
    // condition decides all of it), from each unconditional schema that
    // applies it: the same schema under two conditions is two applications.
    // One that applies unconditionally too is already counted, and so is
    // what it applies.
    let unconditional = result.unconditional.iter().cloned().collect::<HashSet<_>>();
    let mut visited = HashSet::new();
    while let Some((pointer, keyword, origin, evaluates)) = on_a_condition.pop_front() {
      if unconditional.contains(&pointer) || !visited.insert((pointer.clone(), origin.clone(), evaluates)) {
        continue;
      }
      let Some(object) = schema_at(&pointer) else {
        continue;
      };
      if result.whole_object.is_none()
        && let Some(keyword) = whole_object_keyword(object)
      {
        result.whole_object = Some((pointer.clone(), keyword));
      }
      // what an override can change: which properties the table has
      // (`properties`, and what it says of the others, an
      // `additionalProperties: true` included, as that evaluates them for
      // an `unevaluatedProperties` outside the branch) and whether it has
      // one (`required`, `dependentRequired`), which decides a `oneOf`, an
      // `if`, a `not` or a dependency, too
      if result.conditional_properties.is_none()
        && ["required", "dependentRequired"]
          .iter()
          .chain(EVALUATES_PROPERTIES)
          .any(|keyword| object.contains_key(*keyword))
      {
        result.conditional_properties = Some((pointer.clone(), keyword));
      }
      if evaluates && EVALUATES_PROPERTIES.iter().any(|keyword| object.contains_key(*keyword)) {
        result.evaluating_on_a_condition.insert(origin.clone());
      }
      let applies = Applies::of(resource, &pointer, object);
      if let Some(reference) = applies.not_followed {
        result.not_followed.get_or_insert(reference);
        result.unfollowed.insert(pointer.clone());
      }
      on_a_condition.extend(applies.always.into_iter().map(|target| (target, keyword, origin.clone(), evaluates)));
      on_a_condition.extend(
        applies
          .on_a_condition
          .into_iter()
          .map(|(target, keyword)| (target, keyword, origin.clone(), evaluates && keyword != "not")),
      );
    }
    result
  }

  /// The schema at `pointer` and the ones it applies to the table
  /// unconditionally, in turn: what its `unevaluatedProperties` sees.
  fn evaluated_by(&self, pointer: &str) -> Vec<String> {
    let mut result = vec![pointer.to_string()];
    let mut index = 0;
    while index < result.len() {
      if let Some(applied) = self.applied.get(&result[index]) {
        let new = applied.iter().filter(|applied| !result.contains(applied)).cloned().collect::<Vec<_>>();
        result.extend(new);
      }
      index += 1;
    }
    result
  }
}

/// The schema of an override of a plugin's table, which is `files` and the
/// plugin's own properties (what dprint hands the plugin for the files, so
/// nothing is required): each property as the plugin's schema has it in the
/// schemas that apply to the table unconditionally (see
/// [`TableSchemas::unconditional`]), referred to where it is in the plugin's
/// resource, and what each of those schemas says of the other properties
/// (`additionalProperties`, `unevaluatedProperties`), kept to that schema:
/// one that allows no others forbids a property another declares, as in the
/// plugin's schema. What a schema says on a condition (ex. an `anyOf`
/// branch) isn't in it, as the table an override is merged into decides the
/// condition, which is warned about (see
/// [`TableSchemas::conditional_properties`]).
fn override_schema(resource: &Resource, table: &TableSchemas, table_properties: &Map<String, Value>, files: &Value) -> Value {
  // the plugin's own properties a schema declares
  let declared = |pointer: &str| -> Vec<String> {
    match resource.schema.pointer(pointer).and_then(|schema| schema.get("properties")) {
      Some(Value::Object(declared)) => declared.keys().filter(|name| !table_properties.contains_key(*name)).cloned().collect(),
      _ => Vec::new(),
    }
  };
  let mut properties = Map::new();
  let mut restrictions = Vec::new();
  for pointer in &table.unconditional {
    let Some(Value::Object(object)) = resource.schema.pointer(pointer) else {
      continue;
    };
    for name in declared(pointer) {
      let reference = serde_json::json!({ "$ref": resource.index.reference_to(&pointer::append(&format!("{}/properties", pointer), &name)) });
      match properties.get_mut(&name) {
        None => {
          properties.insert(name, reference);
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
    // what this schema says of the properties the schemas in `evaluated`
    // don't declare; `files` is dprint's, so it's exempt
    let others = |keyword: &str| match object.get(keyword) {
      Some(Value::Bool(false)) => Some(Value::Bool(false)),
      Some(Value::Object(_)) => Some(serde_json::json!({ "$ref": resource.index.reference_to(&format!("{}/{}", pointer, keyword)) })),
      _ => None,
    };
    let restriction = |evaluated: &[String], others: Value| {
      let mut allowed = Map::new();
      allowed.insert("files".to_string(), Value::Bool(true));
      for name in evaluated.iter().flat_map(|pointer| declared(pointer)) {
        allowed.insert(name, Value::Bool(true));
      }
      serde_json::json!({ "properties": allowed, "additionalProperties": others })
    };
    // `additionalProperties` is about the properties the schema itself
    // doesn't declare
    if let Some(others) = others("additionalProperties") {
      restrictions.push(restriction(std::slice::from_ref(pointer), others));
    }
    // `unevaluatedProperties` is about the properties nothing the schema
    // applies evaluates: the ones none of those schemas declares, as long as
    // none of them evaluates every property (an `additionalProperties` or
    // `unevaluatedProperties` of its own does, whatever it says, as a
    // validator's annotations go), none of them applies, on a condition, a
    // schema that evaluates properties (what's evaluated then depends on
    // the table the override is merged into, see
    // [`TableSchemas::evaluating_on_a_condition`]), and none of them
    // applies what isn't followed (see [`TableSchemas::unfollowed`]).
    // Otherwise it's left to the plugin rather than guessed.
    if let Some(others) = others("unevaluatedProperties") {
      let evaluated = table.evaluated_by(pointer);
      let evaluates_every_property = evaluated.iter().any(|applied| {
        resource
          .schema
          .pointer(applied)
          .and_then(Value::as_object)
          .is_some_and(|schema| schema.contains_key("additionalProperties") || (applied != pointer && schema.contains_key("unevaluatedProperties")))
      });
      let evaluates_on_a_condition = evaluated.iter().any(|applied| table.evaluating_on_a_condition.contains(applied));
      let applies_whats_not_followed = evaluated.iter().any(|applied| table.unfollowed.contains(applied));
      if !evaluates_every_property && !evaluates_on_a_condition && !applies_whats_not_followed {
        restrictions.push(restriction(&evaluated, others));
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
  if !restrictions.is_empty() {
    result.insert("allOf".to_string(), Value::Array(restrictions));
  }
  Value::Object(result)
}

/// dprint's `overrides` property of a plugin's table, with each override
/// checked by the schema at `reference` rather than the one for any plugin:
/// the property as dprint's schema has it, with its references to the
/// generic override's schema replaced, so that whatever else it says (ex.
/// that it may be left out as `null`) stays as the model says.
fn override_table_property(generic: &Value, reference: &str) -> Value {
  const GENERIC_OVERRIDE: &str = "#/$defs/pluginOverride";
  fn replace(value: &mut Value, reference: &str) {
    match value {
      Value::Object(object) => {
        for (key, value) in object.iter_mut() {
          if key == "$ref" && value == GENERIC_OVERRIDE {
            *value = Value::String(reference.to_string());
          } else {
            replace(value, reference);
          }
        }
      }
      Value::Array(values) => values.iter_mut().for_each(|value| replace(value, reference)),
      _ => {}
    }
  }
  let mut property = generic.clone();
  replace(&mut property, reference);
  property
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
      // the model's property, referring to the plugin's own override schema
      let override_reference = format!("{}#/$defs/dprint-override", reference);
      let text = serde_json::to_string(&properties["overrides"]).unwrap();
      properties["overrides"] = serde_json::from_str(&text.replace("#/$defs/pluginOverride", &override_reference)).unwrap();
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
        &json!({ "test": { "a": "a", "associations": "**/*.a", "overrides": [{ "files": "*.a", "a": "b" }] } })
      ),
      Ok(())
    );
    assert!(validate_with_schema(&schema.schema, &json!({ "test": { "b": "one" } })).is_err());
    // `closed` allows no property but its own, so `b`, which only the root
    // declares, isn't allowed in the table nor in an override, as the plugin's
    // schema says (`additionalProperties` is the schema's own)
    assert!(validate_with_schema(&schema.schema, &json!({ "test": { "a": "a", "b": 1 } })).is_err());
    assert!(validate_with_schema(&schema.schema, &json!({ "test": { "overrides": [{ "files": "*.a", "b": 1 }] } })).is_err());
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
    // what the other document declares is evaluated through the reference,
    // so an `unevaluatedProperties` next to it isn't checked in an override,
    // while an `additionalProperties`, about the schema's own properties, is
    let override_schema = |plugin_schema: Value| build(plugin_schema, Some(URL)).schema["$defs"]["plugin:test"]["$defs"]["dprint-override"].clone();
    let unevaluated = override_schema(json!({
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "$ref": "https://example.com/config.json",
      "unevaluatedProperties": false,
    }));
    assert!(unevaluated.get("allOf").is_none(), "{}", unevaluated);
    let closed = override_schema(json!({
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "$ref": "https://example.com/config.json",
      "additionalProperties": false,
    }));
    assert_eq!(closed["allOf"], json!([{ "properties": { "files": true }, "additionalProperties": false }]));
    // a `$dynamicRef` is resolved by what applies the schema, which the
    // table decides, so it isn't followed either
    let dynamic = json!({
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "$dynamicRef": "#node",
      "unevaluatedProperties": false,
      "$defs": { "node": { "$dynamicAnchor": "node", "properties": { "a": { "type": "string" } } } },
    });
    let schema = build(dynamic.clone(), Some(URL));
    assert_eq!(
      schema.warnings,
      vec![format!(
        "The configuration schema of the test plugin ({}) refers to #node for its table, so editors may report dprint's own properties of its table (ex. `associations`) as unknown.",
        URL
      )]
    );
    assert!(override_schema(dynamic).get("allOf").is_none());
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
    // what each schema says of the other properties is kept to that schema:
    // the second allows none but `b`, so it forbids `a`, which the first
    // declares, as it does of the table
    assert_eq!(
      override_schema["allOf"],
      json!([{ "properties": { "files": true, "b": true }, "additionalProperties": false }])
    );
    assert!(override_schema.get("additionalProperties").is_none());
    assert_eq!(validate(combined.clone(), json!({ "overrides": [{ "files": "*.x", "b": 1 }] })), Ok(()));
    assert!(validate(combined.clone(), json!({ "overrides": [{ "files": "*.x", "a": "y", "b": 1 }] })).is_err());
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

  /// Whether a plugin's own schema accepts a table of its own properties
  /// (what dprint hands it), as a validator reading the schema does.
  fn plugin_accepts(plugin_schema: &Value, plugin_properties: &Value) -> bool {
    let mut schemas = boon::Schemas::new();
    let mut compiler = boon::Compiler::new();
    compiler.add_resource(URL, plugin_schema.clone()).unwrap();
    let index = compiler.compile(URL, &mut schemas).unwrap();
    schemas.validate(plugin_properties, index).is_ok()
  }

  #[test]
  fn an_override_validates_exactly_when_the_plugin_accepts_what_it_makes_of_the_table() {
    // an override's properties replace the table's for its files, so the
    // configuration schema accepts an override exactly when the plugin's
    // schema accepts the table that makes (the base tables here are valid)
    let typed_others_in_a_branch = json!({
      "allOf": [
        { "properties": { "a": { "type": "string" } } },
        { "properties": { "b": { "type": "number" } }, "additionalProperties": { "type": "boolean" } },
      ],
    });
    let closed_branch = json!({
      "$ref": "#/definitions/root",
      "definitions": {
        "root": { "allOf": [{ "$ref": "#/definitions/one" }, { "properties": { "b": { "type": "number" } }, "additionalProperties": false }] },
        "one": { "properties": { "a": { "type": "string" } } },
      },
    });
    let closed = json!({
      "type": "object",
      "properties": { "a": { "type": "string" }, "b": { "type": "number" } },
      "required": ["a"],
      "additionalProperties": false,
    });
    let unevaluated = json!({
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "properties": { "a": { "type": "string" } },
      "allOf": [{ "properties": { "b": { "type": "number" } } }],
      "unevaluatedProperties": false,
    });
    // `additionalProperties` evaluates every property it applies to, so
    // `unevaluatedProperties` has nothing left, whatever `properties` declare
    let unevaluated_after_typed_others = json!({
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "properties": { "a": { "type": "string" } },
      "additionalProperties": { "type": "number" },
      "unevaluatedProperties": false,
    });
    let unevaluated_after_an_open_item = json!({
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "properties": { "a": { "type": "string" } },
      "allOf": [{ "additionalProperties": true }],
      "unevaluatedProperties": false,
    });
    // a schema applied twice: each application is what the
    // `unevaluatedProperties` next to it sees, as a validator's annotations
    // go, however many times the schema is applied
    let shared_twice = json!({
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "$defs": { "shared": { "properties": { "a": { "type": "string" } } } },
      "allOf": [{ "$ref": "#/$defs/shared" }, { "$ref": "#/$defs/shared", "unevaluatedProperties": false }],
    });
    // a schema reached on a condition before it's reached unconditionally
    // applies unconditionally all the same
    let shared_by_a_condition_first = json!({
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "$defs": { "mid": { "$ref": "#/$defs/shared" }, "shared": { "properties": { "a": { "type": "string" } } } },
      "anyOf": [{ "$ref": "#/$defs/shared" }, { "type": "object" }],
      "allOf": [{ "$ref": "#/$defs/mid" }],
    });
    // `then` and `else` without an `if` are ignored, whatever they say
    let then_and_else_without_if = json!({
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "properties": { "a": { "type": "string" } },
      "unevaluatedProperties": false,
      "then": { "properties": { "b": { "type": "number" } } },
      "else": { "additionalProperties": true },
    });
    let then_without_if = json!({ "then": { "maxProperties": 0 } });
    let cases = [
      (&then_and_else_without_if, json!({}), json!({ "a": "x" }), true),
      (&then_and_else_without_if, json!({}), json!({ "b": 1 }), false),
      (&then_and_else_without_if, json!({}), json!({ "zzz": 1 }), false),
      (&then_without_if, json!({}), json!({ "a": 1 }), true),
      (&shared_twice, json!({}), json!({ "a": "x" }), true),
      (&shared_twice, json!({}), json!({ "a": 1 }), false),
      (&shared_twice, json!({}), json!({ "zzz": 1 }), false),
      (&shared_by_a_condition_first, json!({}), json!({ "a": "x" }), true),
      (&shared_by_a_condition_first, json!({}), json!({ "a": 1 }), false),
      (&unevaluated_after_typed_others, json!({}), json!({ "b": 1 }), true),
      (&unevaluated_after_typed_others, json!({}), json!({ "b": "x" }), false),
      (&unevaluated_after_typed_others, json!({}), json!({ "a": "x", "b": 2 }), true),
      (&unevaluated_after_an_open_item, json!({}), json!({ "b": "anything" }), true),
      (&closed_branch, json!({}), json!({ "b": 1 }), true),
      (&closed_branch, json!({}), json!({ "a": "x" }), false),
      (&closed_branch, json!({}), json!({ "a": "x", "b": 1 }), false),
      (&closed_branch, json!({ "b": 2 }), json!({ "b": 3 }), true),
      (&typed_others_in_a_branch, json!({}), json!({ "b": 1, "zzz": true }), true),
      (&typed_others_in_a_branch, json!({}), json!({ "zzz": 1 }), false),
      // `a` is declared by one schema, and must be a boolean by the other
      (&typed_others_in_a_branch, json!({}), json!({ "a": "x" }), false),
      (&typed_others_in_a_branch, json!({}), json!({ "a": true }), false),
      (&closed, json!({ "a": "x" }), json!({ "a": "y", "b": 1 }), true),
      (&closed, json!({ "a": "x" }), json!({ "zzz": true }), false),
      (&closed, json!({ "a": "x" }), json!({ "b": "one" }), false),
      (&unevaluated, json!({}), json!({ "b": 1 }), true),
      (&unevaluated, json!({}), json!({ "a": "x", "b": 2 }), true),
      (&unevaluated, json!({}), json!({ "zzz": 1 }), false),
    ];
    for (plugin_schema, base, override_properties, expected) in cases {
      let schema = build((*plugin_schema).clone(), Some(URL));
      assert_eq!(schema.warnings, Vec::<String>::new(), "{}", plugin_schema);
      assert!(plugin_accepts(plugin_schema, &base), "{} {}", plugin_schema, base);
      let mut merged = base.as_object().unwrap().clone();
      merged.extend(override_properties.as_object().unwrap().clone());
      assert_eq!(
        plugin_accepts(plugin_schema, &Value::Object(merged)),
        expected,
        "{} {} {}",
        plugin_schema,
        base,
        override_properties
      );
      let mut table = base.as_object().unwrap().clone();
      let mut override_config = override_properties.as_object().unwrap().clone();
      override_config.insert("files".to_string(), json!("*.x"));
      table.insert("overrides".to_string(), json!([override_config]));
      assert_eq!(
        validate_with_schema(&schema.schema, &json!({ "test": table })).is_ok(),
        expected,
        "{} {} {}",
        plugin_schema,
        base,
        override_properties
      );
    }
  }

  #[test]
  fn applies_only_what_the_plugins_draft_does() {
    // a keyword a draft doesn't know is ignored by it, while 2020-12 would
    // apply it: the composed schema accepts exactly what the plugin's does
    let of_draft = |draft: &str, mut schema: Value| {
      schema["$schema"] = json!(format!("http://json-schema.org/{}/schema#", draft));
      schema
    };
    let conditional = json!({ "if": {}, "then": { "not": {} } });
    let constant = json!({ "properties": { "a": { "const": 1 } } });
    for (plugin_schema, table, expected) in [
      (of_draft("draft-04", conditional.clone()), json!({ "a": 1 }), true),
      (of_draft("draft-07", conditional.clone()), json!({ "a": 1 }), false),
      (of_draft("draft-04", constant.clone()), json!({ "a": 2 }), true),
      (of_draft("draft-07", constant.clone()), json!({ "a": 2 }), false),
      (of_draft("draft-07", constant.clone()), json!({ "a": 1 }), true),
    ] {
      assert_eq!(plugin_accepts(&plugin_schema, &table), expected, "{} {}", plugin_schema, table);
      let schema = build(plugin_schema.clone(), Some(URL));
      assert_eq!(schema.warnings, Vec::<String>::new(), "{}", plugin_schema);
      assert_eq!(
        validate_with_schema(&schema.schema, &json!({ "test": table })).is_ok(),
        expected,
        "{} {}",
        plugin_schema,
        table
      );
    }
    // a reference into what the draft ignores can't be kept, so the schema
    // is referred to by its url
    let schema = build(
      of_draft(
        "draft-04",
        json!({
          "definitions": { "x": { "if": { "properties": { "b": { "type": "string" } } } } },
          "properties": { "a": { "$ref": "#/definitions/x/if/properties/b" } },
        }),
      ),
      Some(URL),
    );
    assert_eq!(
      schema.warnings,
      vec![concat!(
        "The configuration schema of the test plugin (https://plugins.dprint.dev/test/schema.json) refers to #/definitions/x/if/properties/b ",
        "(at /properties/a) into what its JSON schema draft ignores (/definitions/x/if) and 2020-12 would apply, which can't be translated, ",
        "so it's referred to by its url instead. Editors may report dprint's own properties of its table (ex. `associations`) as unknown."
      )]
    );
    assert_eq!(schema.schema["properties"]["test"]["allOf"], json!([{ "$ref": URL }]));
    assert!(schema.schema["$defs"].get("plugin:test").is_none());
  }

  #[test]
  fn follows_a_reference_into_a_translated_tuple() {
    // a draft-07 tuple's items move to `prefixItems`, and a reference into
    // one of them (not just to the tuple) follows
    let schema = build(
      json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "definitions": {
          "pair": { "type": "array", "items": [{ "type": "string" }, { "type": "integer" }], "additionalItems": false },
        },
        "properties": {
          "label": { "$ref": "#/definitions/pair/items/0" },
          "pair": { "$ref": "#/definitions/pair" },
          "rest": { "type": "array", "items": { "$ref": "#/definitions/pair/additionalItems" } },
        },
      }),
      Some(URL),
    );
    assert_eq!(schema.warnings, Vec::<String>::new());
    assert_eq!(
      schema.schema["$defs"]["plugin:test"]["properties"]["label"],
      json!({ "$ref": "#/definitions/pair/prefixItems/0" })
    );
    let validate = |table: Value| validate_with_schema(&schema.schema, &json!({ "test": table }));
    assert_eq!(validate(json!({ "label": "x", "pair": ["x", 1], "rest": [] })), Ok(()));
    assert!(validate(json!({ "label": 1 })).is_err());
    assert!(validate(json!({ "pair": ["x", 1, 2] })).is_err());
    // `additionalItems: false` is `items: false` now, which the reference follows to
    assert!(validate(json!({ "rest": [1] })).is_err());
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
          "The configuration schema of the test plugin (https://plugins.dprint.dev/test/schema.json) says which properties its table has on a condition too ",
          "(`{}`{}), which an override can't be checked on by itself, as the table it's merged into decides it, so editors check an override's ",
          "properties only as far as the schema says unconditionally."
        ),
        keyword,
        if at.is_empty() { String::new() } else { format!(" at {}", at) }
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
      // (the first that says which properties the table has is named, a
      // `required` included)
      (
        json!({ "oneOf": [{ "required": ["a"] }, { "properties": { "b": { "type": "number" } } }] }),
        "oneOf",
        "/oneOf/0",
      ),
      (
        json!({ "if": { "required": ["a"] }, "then": { "properties": { "b": { "type": "number" } } } }),
        "if",
        "/if",
      ),
      (
        json!({ "if": { "type": "object" }, "then": { "properties": { "b": { "type": "number" } } } }),
        "then",
        "/then",
      ),
      (
        json!({ "if": { "type": "object" }, "else": { "additionalProperties": false } }),
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
    // whether the table has a property counts too: the properties an
    // override adds can decide a `oneOf`, an `if`, a `not` or a dependency
    let exactly_one = json!({ "oneOf": [{ "required": ["a"] }, { "required": ["b"] }] });
    let schema = build(exactly_one.clone(), Some(URL));
    assert_eq!(schema.warnings, vec![warning("oneOf", "/oneOf/0")]);
    // (the override validates, while the plugin rejects the table it makes)
    assert_eq!(validate(&exactly_one, json!({ "a": 1, "overrides": [{ "files": "*.x", "b": 1 }] })), Ok(()));
    assert!(!plugin_accepts(&exactly_one, &json!({ "a": 1, "b": 1 })));
    for (plugin_schema, keyword, at) in [
      (json!({ "anyOf": [{ "required": ["a"] }, { "required": ["b"] }] }), "anyOf", "/anyOf/0"),
      (
        json!({ "if": { "required": ["a"] }, "then": { "required": ["b"] }, "else": { "additionalProperties": true } }),
        "if",
        "/if",
      ),
      (json!({ "not": { "required": ["a"] } }), "not", "/not"),
      // a dependency on a property is a condition wherever it is
      (json!({ "dependencies": { "a": ["b"] } }), "dependentRequired", ""),
      (
        json!({ "allOf": [{ "dependentRequired": { "a": ["b"] } }], "$schema": "https://json-schema.org/draft/2020-12/schema" }),
        "dependentRequired",
        "/allOf/0",
      ),
    ] {
      assert_eq!(
        build(plugin_schema.clone(), Some(URL)).warnings,
        vec![warning(keyword, at)],
        "{}",
        plugin_schema
      );
    }
    // a branch that evaluates every property (`additionalProperties`, whatever
    // it says) decides what an `unevaluatedProperties` outside it has left,
    // so it counts too, and the override isn't closed by that
    // `unevaluatedProperties`
    let open_on_a_condition = json!({
      "$schema": "https://json-schema.org/draft/2020-12/schema",
      "properties": { "a": { "type": "string" } },
      "unevaluatedProperties": false,
      "anyOf": [{ "type": "object", "additionalProperties": true }, { "type": "string" }],
    });
    assert_eq!(build(open_on_a_condition.clone(), Some(URL)).warnings, vec![warning("anyOf", "/anyOf/0")]);
    assert!(plugin_accepts(&open_on_a_condition, &json!({ "zzz": 1 })));
    assert_eq!(validate(&open_on_a_condition, json!({ "overrides": [{ "files": "*.x", "zzz": 1 }] })), Ok(()));
    // a condition that says nothing about the table's properties is fine
    for plugin_schema in [
      json!({ "anyOf": [{ "type": "object" }, { "title": "x" }] }),
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
      // (the dependency on `a` is a condition an override can decide, which
      // is warned about, in each draft's syntax)
      let has_dependency = plugin_schema.get("dependencies").is_some() || plugin_schema.get("dependentRequired").is_some();
      assert_eq!(schema.warnings.len(), usize::from(has_dependency), "{}: {:?}", dialect, schema.warnings);
      if has_dependency {
        assert!(
          schema.warnings[0].contains("on a condition too (`dependentRequired`),"),
          "{}",
          schema.warnings[0]
        );
      }
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
