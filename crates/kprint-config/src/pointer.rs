//! JSON pointers (RFC 6901) in uri fragments (RFC 3986).

use percent_encoding::AsciiSet;
use percent_encoding::CONTROLS;
use percent_encoding::percent_decode_str;
use percent_encoding::utf8_percent_encode;

/// What isn't allowed as is in a uri fragment (RFC 3986).
const FRAGMENT: &AsciiSet = &CONTROLS
  .add(b' ')
  .add(b'"')
  .add(b'#')
  .add(b'%')
  .add(b'<')
  .add(b'>')
  .add(b'[')
  .add(b'\\')
  .add(b']')
  .add(b'^')
  .add(b'`')
  .add(b'{')
  .add(b'|')
  .add(b'}');

/// A reference to the JSON pointer within the schema it's in.
pub fn fragment_reference(pointer: &str) -> String {
  format!("#{}", utf8_percent_encode(pointer, FRAGMENT))
}

/// A reference to the JSON pointer within the schema resource at `uri`.
pub fn resource_reference(uri: &str, pointer: &str) -> String {
  format!("{}{}", uri, fragment_reference(pointer))
}

pub fn decode_fragment(fragment: &str) -> String {
  percent_decode_str(fragment).decode_utf8_lossy().into_owned()
}

/// Escapes a JSON pointer segment (RFC 6901).
pub fn escape_segment(segment: &str) -> String {
  segment.replace('~', "~0").replace('/', "~1")
}

/// Adds a segment to a JSON pointer.
pub fn append(pointer: &str, segment: &str) -> String {
  format!("{}/{}", pointer, escape_segment(segment))
}
