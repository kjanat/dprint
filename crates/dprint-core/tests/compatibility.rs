#[cfg(feature = "formatting")]
#[test]
fn formatting_macro_and_facade_use_the_same_engine() {
  use dprint_core::formatting::PrintItems;
  use dprint_core::formatting::PrintOptions;
  use dprint_core_macros::sc;

  let options = PrintOptions {
    indent_width: 2,
    max_width: 80,
    use_tabs: false,
    new_line_text: "\n",
  };
  let output = dprint_formatting::format(
    || {
      let mut items = PrintItems::new();
      items.push_sc(sc!("hello"));
      items
    },
    options,
  );
  assert_eq!(output, "hello");
}

#[cfg(any(feature = "wasm", feature = "process"))]
#[test]
fn facade_and_direct_plugin_types_preserve_configuration_and_errors() {
  let config = dprint_core::configuration::ConfigKeyMap::from_iter([("key".into(), dprint_core::configuration::ConfigKeyValue::Bool(true))]);
  let payload = dprint_plugin_types::RawFormatConfig {
    plugin: config,
    global: Default::default(),
  };
  let facade_payload: dprint_core::plugins::RawFormatConfig = payload;
  assert_eq!(facade_payload.plugin["key"], dprint_configuration::ConfigKeyValue::Bool(true));

  let error: dprint_core::plugins::FormatError = std::io::Error::other("failed").into();
  let critical = dprint_plugin_types::CriticalFormatError(error);
  let error: dprint_core::plugins::FormatError = critical.into();
  assert!(error.downcast_ref::<dprint_core::plugins::CriticalFormatError>().is_some());
}

#[cfg(feature = "process")]
#[test]
fn asynchronous_requests_preserve_identity_across_transports_and_facade() {
  let request = dprint_plugin_types::HostFormatRequest {
    file_path: "test.txt".into(),
    file_bytes: b"text".to_vec(),
    range: None,
    override_config: Default::default(),
    token: std::sync::Arc::new(dprint_plugin_types::NullCancellationToken),
  };
  let transport: dprint_core::plugins::process::HostFormatRequest = request;
  let facade: dprint_core::plugins::HostFormatRequest = transport;
  assert_eq!(facade.file_bytes, b"text");
}
