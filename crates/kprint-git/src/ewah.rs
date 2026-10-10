use anyhow::Result;
use anyhow::bail;

use crate::reader::Reader;

const WORD_BITS: u64 = 64;

/// `technical/bitmap-format.adoc`, "Appendix A: Serialization format for an EWAH bitmap".
pub(crate) fn read_ewah(reader: &mut Reader, limit: usize) -> Result<Vec<usize>> {
  let bit_count = u64::from(reader.u32()?);
  let word_count = reader.usize32()?;
  let Some(words_len) = word_count.checked_mul(8) else {
    bail!("the EWAH bitmap has too many words");
  };
  let words = reader.bytes(words_len)?;
  reader.skip(4)?;
  let mut words = words.as_chunks::<8>().0.iter().map(|word| u64::from_be_bytes(*word));
  let limit = u64::try_from(limit)?;
  let mut positions = Vec::new();
  let mut push = |position: u64| -> Result<()> {
    if position >= limit {
      bail!("the EWAH bitmap sets bit {position}, beyond {limit}");
    }
    positions.push(usize::try_from(position)?);
    Ok(())
  };
  let mut offset = 0u64;
  let mut remaining_words = word_count;
  while let Some(marker) = words.next() {
    remaining_words -= 1;
    let run_words = (marker >> 1) & 0xffff_ffff;
    let literal_words = marker >> 33;
    // Git's bitmaps show that the run length counts 64-bit words.
    let run_end = offset.saturating_add(run_words * WORD_BITS);
    if marker & 1 == 1 {
      for position in offset..run_end.min(bit_count) {
        push(position)?;
      }
    }
    offset = run_end;
    if literal_words > remaining_words as u64 {
      bail!("the EWAH bitmap has {literal_words} literal words but only {remaining_words} words left");
    }
    for word in words.by_ref().take(literal_words as usize) {
      remaining_words -= 1;
      let mut bits = word;
      while bits != 0 {
        let position = offset.saturating_add(u64::from(bits.trailing_zeros()));
        if position < bit_count {
          push(position)?;
        }
        bits &= bits - 1;
      }
      offset = offset.saturating_add(WORD_BITS);
    }
  }
  Ok(positions)
}

/// Git sizes the bitmaps it writes to end after the last set bit.
#[cfg(any(test, feature = "test-util"))]
pub(crate) fn write_ewah(out: &mut Vec<u8>, positions: &[usize]) {
  let bit_count = positions.iter().max().map_or(0, |last| last + 1);
  let mut literals = vec![0u64; bit_count.div_ceil(64)];
  for position in positions {
    literals[position / 64] |= 1 << (position % 64);
  }
  out.extend_from_slice(&(bit_count as u32).to_be_bytes());
  out.extend_from_slice(&(literals.len() as u32 + 1).to_be_bytes());
  out.extend_from_slice(&((literals.len() as u64) << 33).to_be_bytes());
  for literal in &literals {
    out.extend_from_slice(&literal.to_be_bytes());
  }
  out.extend_from_slice(&0u32.to_be_bytes());
}

#[cfg(test)]
mod test {
  use super::*;

  fn read(bytes: &[u8], limit: usize) -> Result<Vec<usize>> {
    let mut reader = Reader::new(bytes);
    let positions = read_ewah(&mut reader, limit)?;
    assert!(reader.is_empty());
    Ok(positions)
  }

  fn bitmap(bit_count: u32, words: &[u64]) -> Vec<u8> {
    let mut out = bit_count.to_be_bytes().to_vec();
    out.extend_from_slice(&(words.len() as u32).to_be_bytes());
    for word in words {
      out.extend_from_slice(&word.to_be_bytes());
    }
    out.extend_from_slice(&0u32.to_be_bytes());
    out
  }

  fn marker(bit: u64, run_words: u64, literal_words: u64) -> u64 {
    bit | (run_words << 1) | (literal_words << 33)
  }

  #[test]
  fn reads_an_empty_bitmap() {
    assert_eq!(read(&bitmap(0, &[0]), 0).unwrap(), Vec::<usize>::new());
    assert_eq!(read(&bitmap(0, &[]), 0).unwrap(), Vec::<usize>::new());
  }

  #[test]
  fn reads_literal_words() {
    assert_eq!(read(&bitmap(4, &[marker(0, 0, 1), 0b1110]), 4).unwrap(), vec![1, 2, 3]);
    assert_eq!(read(&bitmap(70, &[marker(0, 0, 2), 1, 0b100000]), 70).unwrap(), vec![0, 69]);
  }

  #[test]
  fn reads_runs_of_whole_words() {
    let words = [marker(0, 0, 1), !1, marker(1, 1, 1), 0b110111];
    let mut expected: Vec<usize> = (1..131).collect();
    expected.extend([132, 133]);
    assert_eq!(read(&bitmap(134, &words), 134).unwrap(), expected);
    assert_eq!(read(&bitmap(130, &[marker(0, 2, 1), 1]), 130).unwrap(), vec![128]);
  }

  #[test]
  fn ignores_bits_beyond_the_size() {
    assert_eq!(read(&bitmap(2, &[marker(0, 0, 1), 0b1010]), 2).unwrap(), vec![1]);
    assert_eq!(read(&bitmap(3, &[marker(1, 1, 0)]), 3).unwrap(), vec![0, 1, 2]);
    assert_eq!(read(&bitmap(3, &[marker(1, u64::from(u32::MAX), 0)]), 3).unwrap(), vec![0, 1, 2]);
  }

  #[test]
  fn rejects_bits_beyond_the_limit() {
    assert!(read(&bitmap(4, &[marker(0, 0, 1), 0b1000]), 3).is_err());
    assert!(read(&bitmap(u32::MAX, &[marker(1, u64::from(u32::MAX), 0)]), 10).is_err());
  }

  #[test]
  fn rejects_truncated_bitmaps() {
    let full = bitmap(4, &[marker(0, 0, 1), 0b1110]);
    for len in 0..full.len() {
      assert!(read_ewah(&mut Reader::new(&full[..len]), 4).is_err(), "{len}");
    }
    assert!(read(&bitmap(4, &[marker(0, 0, 2), 0b1110]), 4).is_err());
    let mut huge = bitmap(4, &[]);
    huge[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(read_ewah(&mut Reader::new(&huge), 4).is_err());
  }

  #[test]
  fn round_trips_written_bitmaps() {
    for positions in [vec![], vec![0], vec![1], vec![63], vec![64], vec![0, 5, 63, 64, 200], (0..300).collect()] {
      let mut out = Vec::new();
      write_ewah(&mut out, &positions);
      assert_eq!(read(&out, 1000).unwrap(), positions);
    }
  }

  #[test]
  fn writes_the_sizes_git_writes() {
    let mut out = Vec::new();
    write_ewah(&mut out, &[]);
    assert_eq!(out, bitmap(0, &[0]));
    let mut out = Vec::new();
    write_ewah(&mut out, &[1]);
    assert_eq!(out, bitmap(2, &[marker(0, 0, 1), 0b10]));
  }
}
