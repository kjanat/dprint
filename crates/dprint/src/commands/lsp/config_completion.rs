use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use dprint_config_model::PluginSchema;
use dprint_config_model::SchemaDocument;
use dprint_config_model::build_config_schema;
use dprint_config_model::root_schema;
use jsonc_parser::Scanner;
use jsonc_parser::tokens::Token;
use serde_json::Value;
use text_size::TextSize;
use tower_lsp::lsp_types as lsp;
use url::Url;

use crate::configuration::ConfigFileFormat;
use crate::configuration::POSSIBLE_CONFIG_FILE_NAMES;
use crate::environment::Environment;

use super::config::LspPluginsScopeContainer;
use super::text::LineIndex;

/// Provides completions and hover information for dprint configuration files.
///
/// The schema they come from is the one `dprint schema` generates for the
/// configuration file's plugins (see [`build_config_schema`]): each resolved
/// plugin's configuration schema is fetched (and cached) and composed into
/// it. The actual analysis ([`completions_for`] and [`hover_for`]) is pure
/// and operates only on text + that schema, which keeps it easy to test
/// without a running environment.
pub struct ConfigCompletions<TEnvironment: Environment> {
  environment: TEnvironment,
  scope_container: Rc<LspPluginsScopeContainer<TEnvironment>>,
  /// Cache of plugin config schemas by url. `None` means the url was empty or
  /// the schema failed to download/parse, so we don't keep retrying it.
  schema_cache: RefCell<HashMap<String, Option<Rc<Value>>>>,
}

/// Gets whether the given uri points at a JSON file dprint recognizes as a
/// configuration file (ex. `dprint.json`). The completions are for JSON, so a
/// TOML configuration file is left to a TOML language server.
pub fn is_config_uri(uri: &Url) -> bool {
  let Some(file_name) = uri.path_segments().and_then(|mut s| s.next_back()) else {
    return false;
  };
  POSSIBLE_CONFIG_FILE_NAMES.contains(&file_name) && ConfigFileFormat::from_path(file_name) == ConfigFileFormat::Json
}

impl<TEnvironment: Environment> ConfigCompletions<TEnvironment> {
  pub fn new(environment: TEnvironment, scope_container: Rc<LspPluginsScopeContainer<TEnvironment>>) -> Self {
    Self {
      environment,
      scope_container,
      schema_cache: Default::default(),
    }
  }

  pub async fn completions(&self, file_path: &Path, file_text: &str, position: lsp::Position) -> Option<Vec<lsp::CompletionItem>> {
    let line_index = LineIndex::new(file_text);
    let offset: usize = u32::from(line_index.offset(position).ok()?) as usize;
    let schema = self.build_schema(file_path).await;
    Some(completions_for(&schema, file_text, &line_index, offset))
  }

  pub async fn hover(&self, file_path: &Path, file_text: &str, position: lsp::Position) -> Option<lsp::Hover> {
    let line_index = LineIndex::new(file_text);
    let offset: usize = u32::from(line_index.offset(position).ok()?) as usize;
    let schema = self.build_schema(file_path).await;
    hover_for(&schema, file_text, &line_index, offset)
  }

  async fn build_schema(&self, file_path: &Path) -> ConfigSchema {
    let mut plugins = Vec::new();
    if let Some(parent) = file_path.parent() {
      // a parse error while the user is mid-edit just means we fall back to
      // base-schema-only completions, so ignore any resolution error here
      if let Ok(Some(scope)) = self.scope_container.resolve_by_path(parent).await {
        for plugin in scope.plugins.values() {
          let info = plugin.info();
          let schema = match plugin.plugin.config_schema() {
            // built into dprint, so there's nothing to download
            Some(schema) => serde_json::from_str(schema).ok().map(Rc::new),
            None => self.fetch_schema(&info.config_schema_url).await,
          };
          plugins.push(ConfigPlugin {
            config_key: info.config_key.clone(),
            name: info.name.clone(),
            schema,
            url: Url::parse(&info.config_schema_url).ok(),
          });
        }
      }
    }
    ConfigSchema::new(plugins)
  }

  async fn fetch_schema(&self, url: &str) -> Option<Rc<Value>> {
    let url = url.trim();
    if url.is_empty() {
      return None;
    }
    if let Some(cached) = self.schema_cache.borrow().get(url) {
      return cached.clone();
    }
    let result = self.download_schema(url).await;
    self.schema_cache.borrow_mut().insert(url.to_string(), result.clone());
    result
  }

  async fn download_schema(&self, url: &str) -> Option<Rc<Value>> {
    let parsed_url = Url::parse(url).ok()?;
    match self.environment.download_file_err_404(&parsed_url, None).await {
      Ok((_, file)) => match serde_json::from_slice::<Value>(&file.content) {
        Ok(value) => Some(Rc::new(value)),
        Err(err) => {
          log_debug!(self.environment, "Failed parsing config schema at {}: {:#}", url, err);
          None
        }
      },
      Err(err) => {
        log_debug!(self.environment, "Failed downloading config schema at {}: {:#}", url, err);
        None
      }
    }
  }
}

/// A plugin of the configuration file, with its configuration schema.
struct ConfigPlugin {
  config_key: String,
  name: String,
  /// The plugin's configuration schema. `None` when the plugin doesn't expose
  /// one or it couldn't be downloaded.
  schema: Option<Rc<Value>>,
  url: Option<Url>,
}

/// The configuration file's schema, with the plugins' schemas composed in.
struct ConfigSchema {
  document: SchemaDocument,
  /// The plugins' config keys and names.
  plugins: Vec<(String, String)>,
}

impl ConfigSchema {
  fn new(plugins: Vec<ConfigPlugin>) -> Self {
    let names = plugins.iter().map(|plugin| (plugin.config_key.clone(), plugin.name.clone())).collect();
    let schemas = plugins
      .into_iter()
      .map(|plugin| PluginSchema {
        config_key: plugin.config_key,
        // a plugin without one still has dprint's properties of its table
        schema: plugin.schema.map(|schema| (*schema).clone()).unwrap_or(Value::Bool(true)),
        url: plugin.url,
      })
      .collect();
    let root = match build_config_schema(schemas) {
      Ok(schema) => schema.schema,
      Err(_) => root_schema(),
    };
    Self {
      document: SchemaDocument::new(root),
      plugins: names,
    }
  }

  fn root(&self) -> SchemaRef<'_> {
    SchemaRef {
      document: &self.document,
      pointer: String::new(),
    }
  }

  /// Resolves the object or array located at `path` into the schema that
  /// describes it.
  fn schema_for_path(&self, path: &[PathSeg]) -> Option<SchemaRef<'_>> {
    let mut current = self.root();
    for seg in path {
      current = navigate(current, seg)?;
    }
    Some(current)
  }

  /// Collects the property names that can be suggested for the object at
  /// `path`, excluding any already present in `existing_keys`.
  fn name_options(&self, path: &[PathSeg], existing_keys: &[String]) -> Vec<NameOption> {
    let mut options: Vec<NameOption> = Vec::new();
    let Some(schema) = self.schema_for_path(path) else {
      return options;
    };
    for (name, prop) in schema.property_names() {
      if options.iter().any(|option| option.name == name) || existing_keys.contains(&name) {
        continue;
      }
      let prop = prop.deref();
      // at the root, every resolved plugin's config key is a property
      let plugin = path.is_empty().then(|| self.plugins.iter().find(|(key, _)| *key == name)).flatten();
      options.push(match plugin {
        Some((_, plugin_name)) => NameOption {
          name,
          detail: Some("plugin".to_string()),
          documentation: Some(format!("Configuration for the \"{}\" plugin.", plugin_name)),
        },
        None => NameOption {
          name,
          detail: prop.type_label(),
          documentation: prop.description().map(str::to_string),
        },
      });
    }
    options
  }

  /// Collects the value suggestions for the property `key` of the object at
  /// `path` (ex. the variants of an enum, or `true`/`false`). When `key` is
  /// `None` the suggestions are for an array element.
  fn value_options(&self, path: &[PathSeg], key: Option<&str>) -> Vec<ValueOption> {
    let Some(schema) = self.schema_for_path(path) else {
      return Vec::new();
    };
    let target = match key {
      Some(key) => schema.property(key),
      None => schema.item(),
    };
    target.map(|target| target.value_options()).unwrap_or_default()
  }
}

fn navigate<'a>(schema: SchemaRef<'a>, seg: &PathSeg) -> Option<SchemaRef<'a>> {
  match seg {
    PathSeg::Key(key) => schema.property(key),
    PathSeg::Elem => schema.item(),
  }
}

/// A schema within the configuration schema, by its JSON pointer, which is
/// what its `$ref`s are resolved against (see [`SchemaDocument::resolve`]).
#[derive(Clone)]
struct SchemaRef<'a> {
  document: &'a SchemaDocument,
  pointer: String,
}

impl<'a> SchemaRef<'a> {
  fn node(&self) -> Option<&'a Value> {
    self.document.node(&self.pointer)
  }

  fn child(&self, segment: &str) -> SchemaRef<'a> {
    SchemaRef {
      document: self.document,
      pointer: format!("{}/{}", self.pointer, segment.replace('~', "~0").replace('/', "~1")),
    }
  }

  /// Follows any `$ref`.
  fn deref(&self) -> SchemaRef<'a> {
    let mut current = self.clone();
    for _ in 0..10 {
      let Some(Value::String(reference)) = current.node().and_then(|node| node.get("$ref")) else {
        break;
      };
      match self.document.resolve(reference, &current.pointer) {
        Some(target) => {
          current = SchemaRef {
            document: self.document,
            pointer: target,
          }
        }
        None => break,
      }
    }
    current
  }

  /// The schemas combined into this one (deref'd): what its `allOf`,
  /// `anyOf` and `oneOf` list.
  fn branches(&self) -> Vec<SchemaRef<'a>> {
    let me = self.deref();
    let mut branches = Vec::new();
    for keyword in ["allOf", "anyOf", "oneOf"] {
      if let Some(Value::Array(schemas)) = me.node().and_then(|node| node.get(keyword)) {
        branches.extend((0..schemas.len()).map(|index| me.child(keyword).child(&index.to_string())));
      }
    }
    branches
  }

  fn property(&self, key: &str) -> Option<SchemaRef<'a>> {
    let me = self.deref();
    let node = me.node()?;
    if node.get("properties").and_then(|properties| properties.get(key)).is_some() {
      return Some(me.child("properties").child(key));
    }
    for branch in me.branches() {
      if let Some(found) = branch.property(key) {
        return Some(found);
      }
    }
    match node.get("additionalProperties") {
      Some(Value::Object(_)) => Some(me.child("additionalProperties")),
      _ => None,
    }
  }

  fn item(&self) -> Option<SchemaRef<'a>> {
    let me = self.deref();
    let node = me.node()?;
    match node.get("items") {
      // only single-schema arrays are handled (not tuple validation)
      Some(Value::Object(_) | Value::Bool(_)) => Some(me.child("items")),
      _ => me.branches().into_iter().find_map(|branch| branch.item()),
    }
  }

  fn property_names(&self) -> Vec<(String, SchemaRef<'a>)> {
    let me = self.deref();
    let mut result: Vec<(String, SchemaRef<'a>)> = Vec::new();
    if let Some(Value::Object(props)) = me.node().and_then(|node| node.get("properties")) {
      for name in props.keys() {
        result.push((name.clone(), me.child("properties").child(name)));
      }
    }
    for branch in me.branches() {
      for (name, prop) in branch.property_names() {
        if !result.iter().any(|(existing, _)| *existing == name) {
          result.push((name, prop));
        }
      }
    }
    result
  }

  fn value_options(&self) -> Vec<ValueOption> {
    let me = self.deref();
    let mut options: Vec<ValueOption> = Vec::new();
    let push = |options: &mut Vec<ValueOption>, value: &Value, documentation: Option<String>| {
      if let Some(option) = ValueOption::from_json(value, documentation)
        && !options.iter().any(|o| o.insert_text == option.insert_text)
      {
        options.push(option);
      }
    };
    let Some(node) = me.node() else {
      return options;
    };

    if let Some(Value::Array(values)) = node.get("enum") {
      for value in values {
        push(&mut options, value, None);
      }
    }
    if let Some(value) = node.get("const") {
      push(&mut options, value, me.description().map(str::to_string));
    }
    // the options of each alternative, however deep the alternatives go (ex.
    // an optional property's `anyOf` of its `oneOf` of values and `null`)
    for keyword in ["oneOf", "anyOf"] {
      if let Some(Value::Array(branches)) = node.get(keyword) {
        for index in 0..branches.len() {
          for option in me.child(keyword).child(&index.to_string()).value_options() {
            if !options.iter().any(|o| o.insert_text == option.insert_text) {
              options.push(option);
            }
          }
        }
      }
    }
    if options.is_empty() && me.has_type("boolean") {
      push(&mut options, &Value::Bool(true), None);
      push(&mut options, &Value::Bool(false), None);
    }

    options
  }

  fn description(&self) -> Option<&'a str> {
    self.node()?.get("description").and_then(|d| d.as_str())
  }

  fn has_type(&self, name: &str) -> bool {
    match self.node().and_then(|node| node.get("type")) {
      Some(Value::String(s)) => s == name,
      Some(Value::Array(arr)) => arr.iter().any(|v| v.as_str() == Some(name)),
      _ => false,
    }
  }

  fn type_label(&self) -> Option<String> {
    let node = self.node()?;
    match node.get("type") {
      Some(Value::String(s)) => Some(s.clone()),
      // (`null` reads as the property left out, so it isn't a type of its value)
      Some(Value::Array(arr)) => {
        let joined = arr
          .iter()
          .filter_map(|v| v.as_str())
          .filter(|kind| *kind != "null")
          .collect::<Vec<_>>()
          .join(" | ");
        (!joined.is_empty()).then_some(joined)
      }
      _ => {
        if node.get("enum").is_some() || node.get("oneOf").is_some() || node.get("anyOf").is_some() {
          Some("enum".to_string())
        } else {
          None
        }
      }
    }
  }
}

struct NameOption {
  name: String,
  detail: Option<String>,
  documentation: Option<String>,
}

struct ValueOption {
  /// The text inserted into the document (valid JSON, ex. `"auto"` or `true`).
  insert_text: String,
  /// The label/filter text shown to the user (ex. `auto`).
  display: String,
  documentation: Option<String>,
}

impl ValueOption {
  fn from_json(value: &Value, documentation: Option<String>) -> Option<Self> {
    let insert_text = serde_json::to_string(value).ok()?;
    let display = match value {
      Value::String(s) => s.clone(),
      _ => insert_text.clone(),
    };
    Some(ValueOption {
      insert_text,
      display,
      documentation,
    })
  }
}

// === Pure analysis ===

#[derive(Debug, Clone, PartialEq)]
enum PathSeg {
  Key(String),
  Elem,
}

/// A scanned token with its byte range and whether it can be "edited" (ie. the
/// cursor being inside it means the user is typing that value).
struct Tok {
  kind: TokKind,
  start: usize,
  end: usize,
}

#[derive(Clone)]
enum TokKind {
  OpenBrace,
  CloseBrace,
  OpenBracket,
  CloseBracket,
  Comma,
  Colon,
  /// A string literal, with its decoded contents.
  Str(String),
  /// A bare word (loose property name or partially typed value).
  Word(String),
  /// A scalar that can't be a key in valid usage (boolean/number/null).
  Scalar(String),
  Comment,
}

impl Tok {
  fn is_editable(&self) -> bool {
    matches!(self.kind, TokKind::Str(_) | TokKind::Word(_) | TokKind::Scalar(_))
  }

  fn is_string(&self) -> bool {
    matches!(self.kind, TokKind::Str(_))
  }

  fn scalar_text(&self) -> Option<&str> {
    match &self.kind {
      TokKind::Str(s) | TokKind::Word(s) | TokKind::Scalar(s) => Some(s),
      _ => None,
    }
  }
}

fn scan_tokens(text: &str) -> Vec<Tok> {
  let mut scanner = Scanner::new(text, &Default::default());
  let mut tokens = Vec::new();
  // stop at the end of input or the first scan error; whatever tokens preceded
  // it are enough to determine context up to the cursor
  while let Ok(Some(token)) = scanner.scan() {
    let kind = match token {
      Token::OpenBrace => TokKind::OpenBrace,
      Token::CloseBrace => TokKind::CloseBrace,
      Token::OpenBracket => TokKind::OpenBracket,
      Token::CloseBracket => TokKind::CloseBracket,
      Token::Comma => TokKind::Comma,
      Token::Colon => TokKind::Colon,
      Token::String(value) => TokKind::Str(value.into_owned()),
      Token::Word(value) => TokKind::Word(value.to_string()),
      Token::Boolean(value) => TokKind::Scalar(value.to_string()),
      Token::Number(value) => TokKind::Scalar(value.to_string()),
      Token::Null => TokKind::Scalar("null".to_string()),
      Token::CommentLine(_) | Token::CommentBlock(_) => TokKind::Comment,
    };
    tokens.push(Tok {
      kind,
      start: scanner.token_start(),
      end: scanner.token_end(),
    });
  }
  tokens
}

#[derive(Debug)]
enum Frame {
  Object {
    key_in_parent: Option<PathSeg>,
    last_key: Option<String>,
    after_colon: bool,
    keys: Vec<String>,
  },
  Array {
    key_in_parent: Option<PathSeg>,
  },
}

enum Position {
  /// Completing/hovering a property name.
  ObjectKey,
  /// Completing/hovering the value of `key`.
  ObjectValue { key: String },
  /// Completing/hovering an array element value.
  ArrayValue,
}

struct Analysis {
  /// Path to the innermost container the cursor is in.
  container_path: Vec<PathSeg>,
  position: Position,
  /// Keys already present in the innermost object (before the cursor).
  existing_keys: Vec<String>,
  /// The byte range that an accepted completion should replace.
  replace_range: (usize, usize),
  /// Whether the cursor sits inside a quoted string.
  in_string: bool,
}

fn analyze(tokens: &[Tok], offset: usize) -> Option<Analysis> {
  // the token the cursor is editing (cursor within an editable token)
  let edit_idx = tokens.iter().position(|t| t.is_editable() && t.start <= offset && offset <= t.end);
  let boundary = edit_idx.unwrap_or_else(|| tokens.iter().position(|t| t.end > offset).unwrap_or(tokens.len()));

  let mut stack: Vec<Frame> = Vec::new();
  for tok in &tokens[..boundary] {
    match &tok.kind {
      TokKind::OpenBrace => {
        let key_in_parent = key_in_parent(&stack);
        stack.push(Frame::Object {
          key_in_parent,
          last_key: None,
          after_colon: false,
          keys: Vec::new(),
        });
      }
      TokKind::OpenBracket => {
        let key_in_parent = key_in_parent(&stack);
        stack.push(Frame::Array { key_in_parent });
      }
      TokKind::CloseBrace | TokKind::CloseBracket => {
        stack.pop();
        // the closed container was a completed value of its parent property
        if let Some(Frame::Object { last_key, after_colon, .. }) = stack.last_mut() {
          *last_key = None;
          *after_colon = false;
        }
      }
      TokKind::Colon => {
        if let Some(Frame::Object { after_colon, .. }) = stack.last_mut() {
          *after_colon = true;
        }
      }
      TokKind::Comma => {
        if let Some(Frame::Object { last_key, after_colon, .. }) = stack.last_mut() {
          *last_key = None;
          *after_colon = false;
        }
      }
      TokKind::Comment => {}
      TokKind::Str(_) | TokKind::Word(_) | TokKind::Scalar(_) => {
        if let Some(Frame::Object {
          last_key, after_colon, keys, ..
        }) = stack.last_mut()
        {
          if *after_colon {
            // a scalar value completes the property
            *last_key = None;
            *after_colon = false;
          } else {
            let text = tok.scalar_text().unwrap_or("").to_string();
            keys.push(text.clone());
            *last_key = Some(text);
          }
        }
      }
    }
  }

  let container_path = container_path(&stack);
  let (position, existing_keys) = match stack.last() {
    Some(Frame::Object {
      last_key, after_colon, keys, ..
    }) => {
      let position = match (after_colon, last_key) {
        (true, Some(key)) => Position::ObjectValue { key: key.clone() },
        _ => Position::ObjectKey,
      };
      (position, keys.clone())
    }
    Some(Frame::Array { .. }) => (Position::ArrayValue, Vec::new()),
    // not inside any container (ex. empty document)
    None => return None,
  };

  let (replace_range, in_string) = match edit_idx {
    Some(idx) => ((tokens[idx].start, tokens[idx].end), tokens[idx].is_string()),
    None => ((offset, offset), false),
  };

  Some(Analysis {
    container_path,
    position,
    existing_keys,
    replace_range,
    in_string,
  })
}

fn key_in_parent(stack: &[Frame]) -> Option<PathSeg> {
  match stack.last() {
    Some(Frame::Object { last_key, .. }) => last_key.clone().map(PathSeg::Key),
    Some(Frame::Array { .. }) => Some(PathSeg::Elem),
    None => None,
  }
}

fn container_path(stack: &[Frame]) -> Vec<PathSeg> {
  stack
    .iter()
    .filter_map(|frame| match frame {
      Frame::Object { key_in_parent, .. } => key_in_parent.clone(),
      Frame::Array { key_in_parent } => key_in_parent.clone(),
    })
    .collect()
}

fn completions_for(schema: &ConfigSchema, text: &str, line_index: &LineIndex, offset: usize) -> Vec<lsp::CompletionItem> {
  let tokens = scan_tokens(text);
  let Some(analysis) = analyze(&tokens, offset) else {
    return Vec::new();
  };
  let range = lsp_range(line_index, analysis.replace_range.0, analysis.replace_range.1);

  match &analysis.position {
    Position::ObjectKey => {
      let options = schema.name_options(&analysis.container_path, &analysis.existing_keys);
      options
        .into_iter()
        .map(|option| {
          let new_text = format!("\"{}\"", option.name);
          let filter_text = if analysis.in_string { new_text.clone() } else { option.name.clone() };
          lsp::CompletionItem {
            label: option.name,
            kind: Some(lsp::CompletionItemKind::PROPERTY),
            detail: option.detail,
            documentation: option.documentation.map(markdown),
            filter_text: Some(filter_text),
            text_edit: Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit { range, new_text })),
            ..Default::default()
          }
        })
        .collect()
    }
    Position::ObjectValue { key } => value_items(schema, &analysis, range, Some(key)),
    Position::ArrayValue => value_items(schema, &analysis, range, None),
  }
}

fn value_items(schema: &ConfigSchema, analysis: &Analysis, range: lsp::Range, key: Option<&str>) -> Vec<lsp::CompletionItem> {
  schema
    .value_options(&analysis.container_path, key)
    .into_iter()
    .map(|option| {
      let filter_text = if analysis.in_string {
        option.insert_text.clone()
      } else {
        option.display.clone()
      };
      lsp::CompletionItem {
        label: option.display,
        kind: Some(lsp::CompletionItemKind::VALUE),
        documentation: option.documentation.map(markdown),
        filter_text: Some(filter_text),
        text_edit: Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
          range,
          new_text: option.insert_text,
        })),
        ..Default::default()
      }
    })
    .collect()
}

fn hover_for(schema: &ConfigSchema, text: &str, line_index: &LineIndex, offset: usize) -> Option<lsp::Hover> {
  let tokens = scan_tokens(text);
  // find the token under the cursor (inclusive of its end so hovering the last
  // character still resolves)
  let idx = tokens.iter().position(|t| t.is_editable() && t.start <= offset && offset <= t.end)?;
  let analysis = analyze(&tokens, tokens[idx].start)?;

  let container = schema.schema_for_path(&analysis.container_path)?;
  let target = match &analysis.position {
    // the token is a property name
    Position::ObjectKey => {
      let key = tokens[idx].scalar_text()?;
      container.property(key)?
    }
    // the token is a value
    Position::ObjectValue { key } => container.property(key)?,
    Position::ArrayValue => container.item()?,
  };
  let target = target.deref();

  let mut markdown_text = String::new();
  if let Some(type_label) = target.type_label() {
    markdown_text.push_str(&format!("*{}*\n\n", type_label));
  }
  if let Some(description) = target.description() {
    markdown_text.push_str(description);
  }
  if markdown_text.trim().is_empty() {
    return None;
  }

  Some(lsp::Hover {
    contents: lsp::HoverContents::Markup(lsp::MarkupContent {
      kind: lsp::MarkupKind::Markdown,
      value: markdown_text,
    }),
    range: Some(lsp_range(line_index, tokens[idx].start, tokens[idx].end)),
  })
}

fn lsp_range(line_index: &LineIndex, start: usize, end: usize) -> lsp::Range {
  lsp::Range {
    start: line_index.position_utf16(TextSize::from(start as u32)),
    end: line_index.position_utf16(TextSize::from(end as u32)),
  }
}

fn markdown(value: String) -> lsp::Documentation {
  lsp::Documentation::MarkupContent(lsp::MarkupContent {
    kind: lsp::MarkupKind::Markdown,
    value,
  })
}

#[cfg(test)]
mod test {
  use super::*;

  /// Splits a `%`-marked string into its text and the cursor's byte offset.
  fn at_cursor(text_with_marker: &str) -> (String, usize) {
    let offset = text_with_marker.find('%').expect("missing % cursor marker");
    (text_with_marker.replacen('%', "", 1), offset)
  }

  fn base_only() -> ConfigSchema {
    ConfigSchema::new(Vec::new())
  }

  fn typescript_plugin(schema: Option<Value>) -> ConfigPlugin {
    ConfigPlugin {
      config_key: "typescript".to_string(),
      name: "TypeScript".to_string(),
      schema: schema.map(Rc::new),
      url: Some(Url::parse("https://plugins.dprint.dev/typescript/schema.json").unwrap()),
    }
  }

  fn with_typescript_plugin() -> ConfigSchema {
    ConfigSchema::new(vec![typescript_plugin(Some(serde_json::json!({
      "$schema": "http://json-schema.org/draft-07/schema#",
      "type": "object",
      "definitions": {
        "semiColons": {
          "type": "string",
          "description": "How to use semi-colons.",
          "oneOf": [
            { "const": "always", "description": "Always uses semi-colons." },
            { "const": "asNeeded", "description": "Only when necessary." }
          ]
        }
      },
      "properties": {
        "semiColons": { "$ref": "#/definitions/semiColons" },
        "lineWidth": { "type": "number", "description": "Plugin specific line width." }
      }
    })))])
  }

  fn complete(schema: &ConfigSchema, text_with_marker: &str) -> Vec<lsp::CompletionItem> {
    let (text, offset) = at_cursor(text_with_marker);
    completions_for(schema, &text, &LineIndex::new(&text), offset)
  }

  fn labels(items: &[lsp::CompletionItem]) -> Vec<String> {
    items.iter().map(|item| item.label.clone()).collect()
  }

  fn new_text(item: &lsp::CompletionItem) -> &str {
    match item.text_edit.as_ref().unwrap() {
      lsp::CompletionTextEdit::Edit(edit) => &edit.new_text,
      _ => unreachable!(),
    }
  }

  fn item<'a>(items: &'a [lsp::CompletionItem], label: &str) -> &'a lsp::CompletionItem {
    items
      .iter()
      .find(|i| i.label == label)
      .unwrap_or_else(|| panic!("missing completion: {}", label))
  }

  #[test]
  fn completes_the_built_in_exec_properties_without_downloading() {
    use crate::environment::TestEnvironmentBuilder;
    use crate::plugins::PluginCache;
    use crate::plugins::PluginResolver;

    // no plugin files are served, so this would fail if it tried to download
    let environment = TestEnvironmentBuilder::new()
      .write_file(
        "/dprint.json",
        r#"{
  "plugins": ["https://plugins.dprint.dev/exec-0.7.3.json@a7898d5f1897e77bff474cec3d948c3ec3a7f455e32de2cc60c8adb9a5dd24aa"],
  "exec": { "commands": [{ "command": "tr a-z A-Z", "exts": ["txt"] }] }
}"#,
      )
      .build();
    environment.clone().run_in_runtime(async move {
      let plugin_resolver = Rc::new(PluginResolver::new(environment.clone(), PluginCache::new(environment.clone())));
      let scope_container = Rc::new(LspPluginsScopeContainer::new(environment.clone(), plugin_resolver, None));
      let completions = ConfigCompletions::new(environment.clone(), scope_container);
      let items = completions
        .completions(Path::new("/dprint.json"), r#"{ "exec": {  } }"#, lsp::Position::new(0, 12))
        .await
        .unwrap();
      let labels = labels(&items);
      // what only the built-in exec has
      assert!(labels.contains(&"playWithFire".to_string()), "{:?}", labels);
      assert!(labels.contains(&"setupTimeout".to_string()), "{:?}", labels);
      // and dprint's properties of every plugin table
      assert!(labels.contains(&"associations".to_string()), "{:?}", labels);
    });
  }

  #[test]
  fn completes_root_property_names() {
    let items = complete(&base_only(), "{\n  %\n}");
    let labels = labels(&items);
    assert!(labels.contains(&"lineWidth".to_string()));
    assert!(labels.contains(&"plugins".to_string()));
    assert!(labels.contains(&"newLineKind".to_string()));
    // a property name should be inserted quoted
    assert_eq!(new_text(item(&items, "lineWidth")), "\"lineWidth\"");
    assert_eq!(item(&items, "lineWidth").kind, Some(lsp::CompletionItemKind::PROPERTY));
    assert_eq!(item(&items, "lineWidth").detail.as_deref(), Some("integer"));
  }

  #[test]
  fn completes_partial_property_name_inside_string() {
    let items = complete(&base_only(), "{ \"line%\" }");
    let line_width = item(&items, "lineWidth");
    // the whole string token (including quotes) is replaced
    assert_eq!(new_text(line_width), "\"lineWidth\"");
    assert_eq!(line_width.filter_text.as_deref(), Some("\"lineWidth\""));
  }

  #[test]
  fn excludes_already_present_keys() {
    let items = complete(&base_only(), "{ \"lineWidth\": 80, % }");
    let labels = labels(&items);
    assert!(!labels.contains(&"lineWidth".to_string()));
    assert!(labels.contains(&"indentWidth".to_string()));
  }

  #[test]
  fn completes_enum_values() {
    let items = complete(&base_only(), "{ \"newLineKind\": % }");
    let labels = labels(&items);
    assert_eq!(labels, vec!["auto", "crlf", "lf", "system"]);
    assert_eq!(new_text(item(&items, "auto")), "\"auto\"");
    assert_eq!(item(&items, "auto").kind, Some(lsp::CompletionItemKind::VALUE));
    assert!(item(&items, "crlf").documentation.is_some());
  }

  #[test]
  fn completes_enum_values_inside_string() {
    let items = complete(&base_only(), "{ \"newLineKind\": \"sys%\" }");
    let system = item(&items, "system");
    assert_eq!(new_text(system), "\"system\"");
    assert_eq!(system.filter_text.as_deref(), Some("\"system\""));
  }

  #[test]
  fn completes_boolean_values() {
    let items = complete(&base_only(), "{ \"useTabs\": % }");
    assert_eq!(labels(&items), vec!["true", "false"]);
    assert_eq!(new_text(item(&items, "true")), "true");
  }

  #[test]
  fn completes_plugin_config_key_at_root() {
    let items = complete(&with_typescript_plugin(), "{\n  %\n}");
    let typescript = item(&items, "typescript");
    assert_eq!(new_text(typescript), "\"typescript\"");
    assert_eq!(typescript.detail.as_deref(), Some("plugin"));
    assert!(
      matches!(&typescript.documentation, Some(lsp::Documentation::MarkupContent(content)) if content.value == "Configuration for the \"TypeScript\" plugin.")
    );
  }

  #[test]
  fn completes_plugin_section_property_names() {
    let items = complete(&with_typescript_plugin(), "{ \"typescript\": { % } }");
    let labels = labels(&items);
    // the plugin's own properties
    assert!(labels.contains(&"semiColons".to_string()));
    // plus the properties common to every plugin section
    assert!(labels.contains(&"locked".to_string()));
    assert!(labels.contains(&"associations".to_string()));
  }

  #[test]
  fn completes_plugin_nested_enum_values() {
    // (through the plugin schema's own reference)
    let items = complete(&with_typescript_plugin(), "{ \"typescript\": { \"semiColons\": % } }");
    assert_eq!(labels(&items), vec!["always", "asNeeded"]);
  }

  #[test]
  fn completes_inside_overrides_object() {
    let items = complete(&with_typescript_plugin(), "{ \"typescript\": { \"overrides\": { % } } }");
    let labels = labels(&items);
    // the override's own `files` key
    assert!(labels.contains(&"files".to_string()), "{:?}", labels);
    // plus the plugin's config that can be overridden
    assert!(labels.contains(&"semiColons".to_string()), "{:?}", labels);
  }

  #[test]
  fn completes_inside_overrides_array_element() {
    let items = complete(&with_typescript_plugin(), "{ \"typescript\": { \"overrides\": [{ % }] } }");
    let labels = labels(&items);
    assert!(labels.contains(&"files".to_string()), "{:?}", labels);
    assert!(labels.contains(&"semiColons".to_string()), "{:?}", labels);
  }

  #[test]
  fn completes_overridden_plugin_enum_values() {
    let items = complete(&with_typescript_plugin(), "{ \"typescript\": { \"overrides\": [{ \"semiColons\": % }] } }");
    assert_eq!(labels(&items), vec!["always", "asNeeded"]);
  }

  #[test]
  fn completes_overrides_without_plugin_schema() {
    // a plugin with no schema still offers the common override `files` key
    let schema = ConfigSchema::new(vec![ConfigPlugin {
      config_key: "exec".to_string(),
      name: "Exec".to_string(),
      schema: None,
      url: None,
    }]);
    assert!(labels(&complete(&schema, "{ \"exec\": { \"overrides\": [{ % }] } }")).contains(&"files".to_string()));
    assert!(labels(&complete(&schema, "{ \"exec\": { % } }")).contains(&"associations".to_string()));
    assert_eq!(item(&complete(&schema, "{ % }"), "exec").detail.as_deref(), Some("plugin"));
  }

  #[test]
  fn no_completions_outside_object() {
    assert!(complete(&base_only(), "%").is_empty());
  }

  #[test]
  fn hovers_property_name() {
    let (text, offset) = at_cursor("{ \"newLine%Kind\": \"auto\" }");
    let hover = hover_for(&base_only(), &text, &LineIndex::new(&text), offset).unwrap();
    let lsp::HoverContents::Markup(content) = hover.contents else {
      unreachable!()
    };
    assert!(content.value.contains("The kind of newline to use."));
  }

  #[test]
  fn hovers_plugin_property_value() {
    let (text, offset) = at_cursor("{ \"typescript\": { \"semiColons\": \"alw%ays\" } }");
    let hover = hover_for(&with_typescript_plugin(), &text, &LineIndex::new(&text), offset).unwrap();
    let lsp::HoverContents::Markup(content) = hover.contents else {
      unreachable!()
    };
    assert!(content.value.contains("How to use semi-colons."));
  }
}
