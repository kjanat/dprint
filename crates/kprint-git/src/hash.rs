use sha1::Digest;

pub(crate) fn blob_oid(parts: &[&[u8]], hash_len: usize) -> Option<Vec<u8>> {
  let len: usize = parts.iter().map(|part| part.len()).sum();
  let header = format!("blob {len}\0");
  let mut all = vec![header.as_bytes()];
  all.extend_from_slice(parts);
  digest(&all, hash_len)
}

pub(crate) fn digest(parts: &[&[u8]], hash_len: usize) -> Option<Vec<u8>> {
  fn run<D: Digest>(parts: &[&[u8]]) -> Vec<u8> {
    let mut hasher = D::new();
    for part in parts {
      hasher.update(part);
    }
    hasher.finalize().to_vec()
  }
  match hash_len {
    20 => Some(run::<sha1::Sha1>(parts)),
    32 => Some(run::<sha2::Sha256>(parts)),
    _ => None,
  }
}
