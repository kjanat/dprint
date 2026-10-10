use std::io;
use std::io::Read;
use std::io::Write;

// gitprotocol-common.adoc, "pkt-line Format".
const MAX_PAYLOAD_LEN: usize = 65516;
const HEADER_LEN: usize = 4;
const FLUSH_PACKET: &[u8; 4] = b"0000";

pub fn write_pkt_line_message(writer: &mut impl Write, message: &[u8]) -> io::Result<()> {
  let packet_count = message.len().div_ceil(MAX_PAYLOAD_LEN);
  let mut framed = Vec::with_capacity(message.len() + (packet_count + 1) * HEADER_LEN);
  for payload in message.chunks(MAX_PAYLOAD_LEN) {
    write!(framed, "{:04x}", payload.len() + HEADER_LEN)?;
    framed.extend_from_slice(payload);
  }
  framed.extend_from_slice(FLUSH_PACKET);
  writer.write_all(&framed)?;
  writer.flush()
}

pub fn read_pkt_line_message(reader: &mut impl Read) -> io::Result<Vec<u8>> {
  let mut message = Vec::new();
  loop {
    let mut header = [0; HEADER_LEN];
    reader.read_exact(&mut header)?;
    let len = header
      .iter()
      .try_fold(0usize, |len, byte| Some(len * 16 + char::from(*byte).to_digit(16)? as usize));
    let payload_len = match len {
      Some(0) => return Ok(message),
      Some(len) if (HEADER_LEN..=HEADER_LEN + MAX_PAYLOAD_LEN).contains(&len) => len - HEADER_LEN,
      _ => {
        return Err(io::Error::new(
          io::ErrorKind::InvalidData,
          format!("invalid pkt-line length {:?}", String::from_utf8_lossy(&header)),
        ));
      }
    };
    let start = message.len();
    message.resize(start + payload_len, 0);
    reader.read_exact(&mut message[start..])?;
  }
}

#[cfg(test)]
mod test {
  use super::*;

  fn write(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    write_pkt_line_message(&mut out, message).unwrap();
    out
  }

  fn read(mut bytes: &[u8]) -> io::Result<Vec<u8>> {
    read_pkt_line_message(&mut bytes)
  }

  #[test]
  fn frames_the_documented_examples() {
    assert_eq!(write(b"a\n"), b"0006a\n0000");
    assert_eq!(write(b"a"), b"0005a0000");
    assert_eq!(write(b"foobar\n"), b"000bfoobar\n0000");
    assert_eq!(write(b""), b"0000");
  }

  #[test]
  fn splits_long_messages_at_the_maximum_payload() {
    let message: Vec<u8> = (0..MAX_PAYLOAD_LEN * 2 + 10).map(|index| index as u8).collect();
    let framed = write(&message);
    assert_eq!(&framed[..4], b"fff0");
    assert_eq!(&framed[4 + MAX_PAYLOAD_LEN..8 + MAX_PAYLOAD_LEN], b"fff0");
    assert_eq!(&framed[8 + 2 * MAX_PAYLOAD_LEN..12 + 2 * MAX_PAYLOAD_LEN], b"000e");
    assert_eq!(&framed[framed.len() - 4..], b"0000");
    assert_eq!(framed.len(), message.len() + 4 * 4);
    assert_eq!(read(&framed).unwrap(), message);
    let exact = vec![7; MAX_PAYLOAD_LEN];
    assert_eq!(write(&exact).len(), MAX_PAYLOAD_LEN + 8);
  }

  #[test]
  fn reads_until_the_flush_packet() {
    assert_eq!(read(b"0006a\n0005b0000rest").unwrap(), b"a\nb");
    assert_eq!(read(b"0000").unwrap(), b"");
    assert_eq!(read(b"00040000").unwrap(), b"");
    assert_eq!(read(b"000Bfoobar\n0000").unwrap(), b"foobar\n");
    let mut stream: &[u8] = b"0005a00000005b0000";
    assert_eq!(read_pkt_line_message(&mut stream).unwrap(), b"a");
    assert_eq!(read_pkt_line_message(&mut stream).unwrap(), b"b");
  }

  #[test]
  fn rejects_malformed_lengths() {
    for bytes in [
      &b"0001"[..],
      b"0002",
      b"0003",
      b"fff1",
      b"ffff",
      b"00x5a0000",
      b"-005a0000",
      b"+005a0000",
      b"   5a0000",
    ] {
      assert_eq!(
        read(bytes).unwrap_err().kind(),
        io::ErrorKind::InvalidData,
        "{:?}",
        String::from_utf8_lossy(bytes)
      );
    }
  }

  #[test]
  fn rejects_a_missing_flush_packet() {
    for bytes in [&b""[..], b"00", b"0006a\n", b"0006a", b"0006a\n000"] {
      assert_eq!(
        read(bytes).unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof,
        "{:?}",
        String::from_utf8_lossy(bytes)
      );
    }
  }

  #[cfg(unix)]
  #[test]
  fn talks_to_the_fsmonitor_daemon() {
    use std::os::unix::net::UnixStream;

    let Some(repo) = crate::test_git::TempRepo::new(&[]) else {
      return;
    };
    if !repo.try_git(&["fsmonitor--daemon", "start"]).status.success() {
      return;
    }
    struct StopDaemon<'a>(&'a crate::test_git::TempRepo);
    impl Drop for StopDaemon<'_> {
      fn drop(&mut self) {
        self.0.try_git(&["fsmonitor--daemon", "stop"]);
      }
    }
    let _stop = StopDaemon(&repo);
    let ask = |token: &[u8]| {
      let mut stream = UnixStream::connect(repo.path(".git/fsmonitor--daemon.ipc")).unwrap();
      stream.set_read_timeout(Some(std::time::Duration::from_secs(30))).unwrap();
      write_pkt_line_message(&mut stream, token).unwrap();
      read_pkt_line_message(&mut stream).unwrap()
    };
    let response = ask(b"unknown-token");
    let (token, rest) = response.split_at(response.iter().position(|byte| *byte == 0).unwrap());
    assert!(token.starts_with(b"builtin:"), "{:?}", String::from_utf8_lossy(&response));
    assert_eq!(rest, b"\0/\0");

    let long_name = "n".repeat(200);
    for index in 0..700 {
      repo.write(&format!("{long_name}{index}"), b"");
    }
    let mut response = Vec::new();
    for _ in 0..100 {
      response = ask(token);
      if response.len() > MAX_PAYLOAD_LEN * 2 {
        break;
      }
      std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(response.starts_with(b"builtin:"), "{:?}", String::from_utf8_lossy(&response));
    assert!(response.len() > MAX_PAYLOAD_LEN * 2, "{}", response.len());
  }
}
