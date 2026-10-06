//! The JSON schema of a dprint configuration file.
//!
//! dprint's own part of it (the configuration file's properties, and what
//! every plugin's table has) is generated from the types in [`file`], so it
//! can't drift from them. `dprint schema` composes the plugins' schemas in
//! (see [`compose`]), each as a schema resource of its own under `$defs`
//! that keeps its `$id` and its references, as JSON schema 2020-12 bundles
//! them.

pub mod compose;
pub mod document;
pub mod file;
mod generate;
mod pointer;
mod translate;

pub use compose::ConfigSchema;
pub use compose::PluginSchema;
pub use compose::build_config_schema;
pub use document::SchemaDocument;
pub use file::ROOT_SCHEMA_ID;
pub use file::root_schema;
pub use generate::schema_for;
pub use generate::schema_json_for;

/// The name of the schema file `dprint schema` creates for a configuration
/// file. When there's one next to a configuration file, the commands that
/// change its plugins keep it up to date.
pub const CONFIG_SCHEMA_FILE_NAME: &str = "dprint.schema.json";
