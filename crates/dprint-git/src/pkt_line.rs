//! Git's pkt-line framing, as in git's [`pkt-line.c`](https://github.com/git/git/blob/6de20f6092dcf9bdb1c8efe03db4b70c82b423dd/pkt-line.c).

use std::io;
use std::io::Read;
use std::io::Write;

/// Git's `LARGE_PACKET_DATA_MAX`.
const MAX_PACKET_DATA_LEN: usize = 65520 - 4;

pub fn write_pkt_line_message(writer: &mut impl Write, message: &[u8]) -> io::Result<()> {
  for chunk in message.chunks(MAX_PACKET_DATA_LEN) {
    write!(writer, "{:04x}", chunk.len() + 4)?;
    writer.write_all(chunk)?;
  }
  writer.write_all(b"0000")?;
  writer.flush()
}

pub fn read_pkt_line_message(reader: &mut impl Read) -> io::Result<Vec<u8>> {
  let mut message = Vec::new();
  loop {
    let mut header = [0; 4];
    reader.read_exact(&mut header)?;
    let len = std::str::from_utf8(&header)
      .ok()
      .and_then(|text| usize::from_str_radix(text, 16).ok())
      .ok_or_else(|| invalid_data(format!("invalid pkt-line length {:?}", String::from_utf8_lossy(&header))))?;
    match len {
      0 => return Ok(message),
      1..=3 => return Err(invalid_data(format!("unexpected pkt-line control packet {:04x}", len))),
      len => {
        let start = message.len();
        message.resize(start + len - 4, 0);
        reader.read_exact(&mut message[start..])?;
      }
    }
  }
}

fn invalid_data(message: String) -> io::Error {
  io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod test {
  use super::*;

  #[test]
  fn writes_packets_and_a_flush() {
    let mut bytes = Vec::new();
    write_pkt_line_message(&mut bytes, b"builtin:1:2").unwrap();
    assert_eq!(bytes, b"000fbuiltin:1:20000");
  }

  #[test]
  fn writes_an_empty_message_as_a_flush() {
    let mut bytes = Vec::new();
    write_pkt_line_message(&mut bytes, b"").unwrap();
    assert_eq!(bytes, b"0000");
  }

  #[test]
  fn splits_large_messages_into_packets() {
    let message = vec![b'a'; MAX_PACKET_DATA_LEN + 10];
    let mut bytes = Vec::new();
    write_pkt_line_message(&mut bytes, &message).unwrap();
    assert_eq!(&bytes[..4], b"fff0");
    assert_eq!(&bytes[4 + MAX_PACKET_DATA_LEN..4 + MAX_PACKET_DATA_LEN + 4], b"000e");
    assert_eq!(read_pkt_line_message(&mut bytes.as_slice()).unwrap(), message);
  }

  #[test]
  fn reads_packets_up_to_the_flush() {
    let bytes = b"0009abcde0007fgh0000trailing";
    assert_eq!(read_pkt_line_message(&mut bytes.as_slice()).unwrap(), b"abcdefgh");
  }

  #[test]
  fn errors_on_end_of_file_before_the_flush() {
    let err = read_pkt_line_message(&mut b"0009abcde".as_slice()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
  }

  #[test]
  fn errors_on_invalid_lengths() {
    assert_eq!(read_pkt_line_message(&mut b"zzzz".as_slice()).unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert_eq!(read_pkt_line_message(&mut b"0001".as_slice()).unwrap_err().kind(), io::ErrorKind::InvalidData);
  }
}
