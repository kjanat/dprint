//! Git's EWAH compressed bitmaps ([`ewah/ewah_io.c`], [`ewah/ewah_bitmap.c`]).
//!
//! [`ewah/ewah_io.c`]: https://github.com/git/git/blob/6de20f6092dcf9bdb1c8efe03db4b70c82b423dd/ewah/ewah_io.c
//! [`ewah/ewah_bitmap.c`]: https://github.com/git/git/blob/6de20f6092dcf9bdb1c8efe03db4b70c82b423dd/ewah/ewah_bitmap.c

use anyhow::Result;
use anyhow::bail;

use crate::bytes::ByteReader;

const BITS_IN_WORD: usize = 64;
const RUNNING_LEN_BITS: u32 = 32;

/// Reads a serialized bitmap and returns the positions of its set bits in
/// ascending order.
pub fn read_ewah(reader: &mut ByteReader) -> Result<Vec<usize>> {
  let _bit_size = reader.u32()?;
  let word_count = reader.u32()? as usize;
  let mut words = Vec::with_capacity(word_count.min(1 << 16));
  for _ in 0..word_count {
    words.push(reader.u64()?);
  }
  let _last_running_length_word = reader.u32()?;

  let mut set_bits = Vec::new();
  let mut pos = 0usize;
  let mut index = 0;
  while index < words.len() {
    let marker = words[index];
    index += 1;
    let running_len = ((marker >> 1) & ((1 << RUNNING_LEN_BITS) - 1)) as usize * BITS_IN_WORD;
    if marker & 1 == 1 {
      set_bits.extend(pos..pos + running_len);
    }
    pos += running_len;
    let literal_words = (marker >> (1 + RUNNING_LEN_BITS)) as usize;
    if index + literal_words > words.len() {
      bail!("EWAH bitmap has {} literal words past its end", index + literal_words - words.len());
    }
    for word in &words[index..index + literal_words] {
      for bit in 0..BITS_IN_WORD {
        if word & (1 << bit) != 0 {
          set_bits.push(pos + bit);
        }
      }
      pos += BITS_IN_WORD;
    }
    index += literal_words;
  }
  Ok(set_bits)
}

/// Serializes `set_bits` with every word stored as a literal.
#[cfg(any(test, feature = "test-util"))]
pub fn write_ewah(set_bits: &[usize]) -> Vec<u8> {
  let bit_size = set_bits.iter().max().map(|max| max + 1).unwrap_or(0);
  let literal_words = bit_size.div_ceil(BITS_IN_WORD);
  let mut words = vec![(literal_words as u64) << (1 + RUNNING_LEN_BITS)];
  words.extend(std::iter::repeat_n(0u64, literal_words));
  for bit in set_bits {
    words[1 + bit / BITS_IN_WORD] |= 1 << (bit % BITS_IN_WORD);
  }
  let mut bytes = Vec::new();
  bytes.extend((bit_size as u32).to_be_bytes());
  bytes.extend((words.len() as u32).to_be_bytes());
  for word in words {
    bytes.extend(word.to_be_bytes());
  }
  bytes.extend(0u32.to_be_bytes());
  bytes
}

#[cfg(test)]
mod test {
  use super::*;

  fn read(bytes: &[u8]) -> Vec<usize> {
    let mut reader = ByteReader::new(bytes);
    let bits = read_ewah(&mut reader).unwrap();
    assert!(reader.is_empty());
    bits
  }

  #[test]
  fn round_trips_literal_words() {
    for bits in [vec![], vec![0], vec![1, 5, 63, 64, 200]] {
      assert_eq!(read(&write_ewah(&bits)), bits);
    }
  }

  #[test]
  fn expands_running_words() {
    // a run of 2 words of ones, then 1 literal word with bit 3 set
    let marker: u64 = 1 | (2 << 1) | (1 << 33);
    let mut bytes = Vec::new();
    bytes.extend(200u32.to_be_bytes());
    bytes.extend(2u32.to_be_bytes());
    bytes.extend(marker.to_be_bytes());
    bytes.extend(8u64.to_be_bytes());
    bytes.extend(0u32.to_be_bytes());
    let expected = (0..128).chain([131]).collect::<Vec<_>>();
    assert_eq!(read(&bytes), expected);
  }

  #[test]
  fn skips_running_words_of_zeros() {
    let marker: u64 = (3 << 1) | (1 << 33);
    let mut bytes = Vec::new();
    bytes.extend(256u32.to_be_bytes());
    bytes.extend(2u32.to_be_bytes());
    bytes.extend(marker.to_be_bytes());
    bytes.extend(1u64.to_be_bytes());
    bytes.extend(0u32.to_be_bytes());
    assert_eq!(read(&bytes), vec![192]);
  }

  #[test]
  fn errors_on_literal_words_past_the_end() {
    let marker: u64 = 2 << 33;
    let mut bytes = Vec::new();
    bytes.extend(64u32.to_be_bytes());
    bytes.extend(1u32.to_be_bytes());
    bytes.extend(marker.to_be_bytes());
    bytes.extend(0u32.to_be_bytes());
    assert!(read_ewah(&mut ByteReader::new(&bytes)).is_err());
  }
}
