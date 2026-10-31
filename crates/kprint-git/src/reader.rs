use anyhow::Result;
use anyhow::bail;

pub(crate) struct Reader<'a> {
  data: &'a [u8],
  position: usize,
}

impl<'a> Reader<'a> {
  pub fn new(data: &'a [u8]) -> Self {
    Reader { data, position: 0 }
  }

  pub fn position(&self) -> usize {
    self.position
  }

  pub fn is_empty(&self) -> bool {
    self.position == self.data.len()
  }

  pub fn remaining(&self) -> &'a [u8] {
    &self.data[self.position..]
  }

  pub fn bytes(&mut self, len: usize) -> Result<&'a [u8]> {
    let Some(end) = self.position.checked_add(len).filter(|end| *end <= self.data.len()) else {
      bail!("unexpected end of data at offset {}", self.position);
    };
    let bytes = &self.data[self.position..end];
    self.position = end;
    Ok(bytes)
  }

  pub fn skip(&mut self, len: usize) -> Result<()> {
    self.bytes(len).map(|_| ())
  }

  pub fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
    let mut array = [0; N];
    array.copy_from_slice(self.bytes(N)?);
    Ok(array)
  }

  pub fn u8(&mut self) -> Result<u8> {
    Ok(self.array::<1>()?[0])
  }

  pub fn u16(&mut self) -> Result<u16> {
    Ok(u16::from_be_bytes(self.array()?))
  }

  pub fn u32(&mut self) -> Result<u32> {
    Ok(u32::from_be_bytes(self.array()?))
  }

  pub fn usize32(&mut self) -> Result<usize> {
    Ok(usize::try_from(self.u32()?)?)
  }

  pub fn nul_terminated(&mut self) -> Result<&'a [u8]> {
    let Some(len) = self.remaining().iter().position(|byte| *byte == 0) else {
      bail!("missing NUL terminator after offset {}", self.position);
    };
    let bytes = self.bytes(len)?;
    self.position += 1;
    Ok(bytes)
  }

  /// The "offset encoding" of `gitformat-pack.adoc`, which `gitformat-index.adoc` calls variable width encoding.
  pub fn varint(&mut self) -> Result<usize> {
    let mut byte = self.u8()?;
    let mut value = u64::from(byte & 0x7f);
    while byte & 0x80 != 0 {
      byte = self.u8()?;
      let Some(shifted) = value.checked_add(1).and_then(|value| value.checked_mul(0x80)) else {
        bail!("variable width integer overflows at offset {}", self.position);
      };
      value = shifted | u64::from(byte & 0x7f);
    }
    Ok(usize::try_from(value)?)
  }
}

#[cfg(any(test, feature = "test-util"))]
pub(crate) fn write_varint(out: &mut Vec<u8>, value: usize) {
  let mut value = value as u64;
  let mut bytes = vec![(value & 0x7f) as u8];
  while value >= 0x80 {
    value = (value >> 7) - 1;
    bytes.push(0x80 | (value & 0x7f) as u8);
  }
  out.extend(bytes.iter().rev());
}

#[cfg(test)]
mod test {
  use super::*;

  #[test]
  fn reads_big_endian_numbers() {
    let mut reader = Reader::new(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
    assert_eq!(reader.u8().unwrap(), 1);
    assert_eq!(reader.u16().unwrap(), 0x0203);
    assert_eq!(reader.u32().unwrap(), 0x0405_0607);
    assert_eq!(reader.usize32().unwrap(), 0x0809_0a0b);
    assert!(reader.is_empty());
    assert!(reader.u8().is_err());
  }

  #[test]
  fn reads_nul_terminated_strings() {
    let mut reader = Reader::new(b"ab\0\0c");
    assert_eq!(reader.nul_terminated().unwrap(), b"ab");
    assert_eq!(reader.nul_terminated().unwrap(), b"");
    assert!(reader.nul_terminated().is_err());
    assert_eq!(reader.remaining(), b"c");
  }

  #[test]
  fn rejects_reads_past_the_end() {
    let mut reader = Reader::new(&[1, 2]);
    assert!(reader.bytes(3).is_err());
    assert!(reader.skip(usize::MAX).is_err());
    assert_eq!(reader.position(), 0);
    assert!(reader.u16().is_ok());
  }

  #[test]
  fn reads_the_offset_encoding() {
    for (bytes, value) in [
      (&[0x00][..], 0),
      (&[0x7f][..], 127),
      (&[0x80, 0x00][..], 128),
      (&[0x80, 0x48][..], 200),
      (&[0xff, 0x7f][..], 16511),
      (&[0x80, 0x80, 0x00][..], 16512),
    ] {
      assert_eq!(Reader::new(bytes).varint().unwrap(), value, "{bytes:?}");
      let mut written = Vec::new();
      write_varint(&mut written, value);
      assert_eq!(written, bytes);
    }
  }

  #[test]
  fn round_trips_the_offset_encoding() {
    for value in [0, 1, 127, 128, 255, 16383, 16511, 16512, 1 << 20, 1 << 40, usize::MAX >> 1, usize::MAX] {
      let mut written = Vec::new();
      write_varint(&mut written, value);
      let mut reader = Reader::new(&written);
      assert_eq!(reader.varint().unwrap(), value);
      assert!(reader.is_empty());
    }
  }

  #[test]
  fn rejects_bad_offset_encodings() {
    assert!(Reader::new(&[0x80]).varint().is_err());
    assert!(Reader::new(&[0xff; 11]).varint().is_err());
    assert!(
      Reader::new(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f])
        .varint()
        .is_err()
    );
  }
}
