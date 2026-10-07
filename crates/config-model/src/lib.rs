//! The model of a dprint configuration file: what it may hold, as the types
//! a configuration file is read into (see [`from_values`]) and, with the
//! `schema` feature, that its JSON schema is generated from. What dprint
//! accepts and what the schema says are one definition, so they can't
//! disagree.
//!
//! The model is the file's syntax: the names, types and shapes of dprint's
//! own properties (see [`ConfigFile`]) and of every plugin's table (see
//! [`PluginTable`]). What a plugin's own properties are is the plugin's
//! schema's to say, and what a configuration means once its files are
//! combined is dprint's (see `config_layer.rs` and `resolve_config.rs` in
//! the dprint crate, which convert the model into that).
//!
//! `dprint schema` composes the plugins' schemas into the model's (see
//! [`compose`]), each as a schema resource of its own under `$defs` that
//! keeps its `$id` and its references, as JSON schema 2020-12 bundles them.

pub mod file;
mod values;

#[cfg(feature = "schema")]
pub mod compose;
#[cfg(feature = "schema")]
pub mod document;
#[cfg(feature = "schema")]
mod generate;
#[cfg(feature = "schema")]
mod pointer;
#[cfg(feature = "schema")]
mod translate;

pub use file::Associations;
pub use file::ConfigFile;
pub use file::Extends;
pub use file::GlobalSettings;
pub use file::NewLineKind;
pub use file::OverrideFiles;
pub use file::OverrideProperties;
pub use file::Overrides;
pub use file::PluginOverride;
pub use file::PluginTable;
pub use file::ROOT_SCHEMA_ID;
pub use file::ShebangExtension;
pub use file::Shebangs;
pub use values::ValueError;
pub use values::from_json;
pub use values::from_value;
pub use values::from_values;
pub use values::property_name;
pub use values::to_values;

#[cfg(feature = "schema")]
pub use compose::ConfigSchema;
#[cfg(feature = "schema")]
pub use compose::PluginSchema;
#[cfg(feature = "schema")]
pub use compose::build_config_schema;
#[cfg(feature = "schema")]
pub use document::SchemaDocument;
#[cfg(feature = "schema")]
pub use file::root_schema;
#[cfg(feature = "schema")]
pub use generate::schema_for;
#[cfg(feature = "schema")]
pub use generate::schema_json_for;

/// The name of the schema file `dprint schema` creates for a configuration
/// file. When there's one next to a configuration file, the commands that
/// change its plugins keep it up to date.
pub const CONFIG_SCHEMA_FILE_NAME: &str = "dprint.schema.json";
