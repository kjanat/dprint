use anyhow::Result;
use anyhow::bail;

pub struct ByteReader<'a> {
  data: &'a [u8],
  pos: usize,
}

impl<'a> ByteReader<'a> {
  pub fn new(data: &'a [u8]) -> Self {
    Self { data, pos: 0 }
  }

  pub fn pos(&self) -> usize {
    self.pos
  }

  pub fn is_empty(&self) -> bool {
    self.pos == self.data.len()
  }

  pub fn take(&mut self, len: usize) -> Result<&'a [u8]> {
    let Some(bytes) = self.pos.checked_add(len).and_then(|end| self.data.get(self.pos..end)) else {
      bail!("unexpected end of data at offset {} reading {} bytes", self.pos, len);
    };
    self.pos += len;
    Ok(bytes)
  }

  pub fn skip_to(&mut self, pos: usize) -> Result<()> {
    if pos < self.pos || pos > self.data.len() {
      bail!("invalid offset {}", pos);
    }
    self.pos = pos;
    Ok(())
  }

  pub fn u16(&mut self) -> Result<u16> {
    Ok(u16::from_be_bytes(self.take(2)?.try_into()?))
  }

  pub fn u32(&mut self) -> Result<u32> {
    Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
  }

  pub fn u64(&mut self) -> Result<u64> {
    Ok(u64::from_be_bytes(self.take(8)?.try_into()?))
  }

  /// Reads up to a NUL byte and skips it.
  pub fn c_str(&mut self) -> Result<&'a [u8]> {
    let rest = &self.data[self.pos..];
    let Some(len) = rest.iter().position(|byte| *byte == 0) else {
      bail!("unterminated string at offset {}", self.pos);
    };
    self.pos += len + 1;
    Ok(&rest[..len])
  }

  /// Git's offset varint (`decode_varint` in `varint.c`).
  pub fn varint(&mut self) -> Result<u64> {
    let mut byte = self.take(1)?[0];
    let mut value = u64::from(byte & 127);
    while byte & 128 != 0 {
      value = value
        .checked_add(1)
        .filter(|value| value >> 57 == 0)
        .ok_or_else(|| anyhow::anyhow!("varint overflow"))?;
      byte = self.take(1)?[0];
      value = (value << 7) + u64::from(byte & 127);
    }
    Ok(value)
  }
}

#[cfg(any(test, feature = "test-util"))]
pub fn encode_varint(mut value: u64) -> Vec<u8> {
  let mut bytes = vec![(value & 127) as u8];
  while value >> 7 != 0 {
    value = (value >> 7) - 1;
    bytes.push(128 | (value & 127) as u8);
  }
  bytes.reverse();
  bytes
}

#[cfg(test)]
mod test {
  use super::*;

  #[test]
  fn reads_git_varints() {
    for value in [0, 1, 127, 128, 255, 16383, 16511, 16512, 1 << 40] {
      let bytes = encode_varint(value);
      let mut reader = ByteReader::new(&bytes);
      assert_eq!(reader.varint().unwrap(), value);
      assert!(reader.is_empty());
    }
    // `encode_varint` in git's `varint.c` writes 128 as 0x80 0x00
    assert_eq!(encode_varint(128), vec![0x80, 0x00]);
  }

  #[test]
  fn reads_c_strings() {
    let mut reader = ByteReader::new(b"abc\0\0x");
    assert_eq!(reader.c_str().unwrap(), b"abc");
    assert_eq!(reader.c_str().unwrap(), b"");
    assert!(reader.c_str().is_err());
  }

  #[test]
  fn errors_reading_past_the_end() {
    let mut reader = ByteReader::new(&[1, 2, 3]);
    assert!(reader.u32().is_err());
    assert_eq!(reader.u16().unwrap(), 0x0102);
    assert!(reader.varint().is_ok());
    assert!(reader.varint().is_err());
  }
}
