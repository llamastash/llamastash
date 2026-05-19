//! Tiny hex encoder kept in-crate so we don't take on a dedicated
//! `hex` dependency just to render digests and content hashes.
//!
//! Two consumers landed in v2 (`snapshot::ManagedKey` value digests,
//! and the install path's SHA-256 file/byte hashing), each of which
//! had its own copy of the same nibble loop. This module is the
//! single source.

/// Lower-case hex encode of `bytes`. Output length is `2 * bytes.len()`.
pub fn encode(bytes: &[u8]) -> String {
  let mut out = String::with_capacity(bytes.len() * 2);
  for b in bytes {
    out.push(nibble(b >> 4));
    out.push(nibble(b & 0x0f));
  }
  out
}

fn nibble(n: u8) -> char {
  if n < 10 {
    (b'0' + n) as char
  } else {
    (b'a' + (n - 10)) as char
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn empty_input_is_empty_string() {
    assert_eq!(encode(&[]), "");
  }

  #[test]
  fn known_test_vectors() {
    assert_eq!(encode(&[0x00]), "00");
    assert_eq!(encode(&[0xff]), "ff");
    assert_eq!(encode(&[0xab, 0xcd]), "abcd");
    assert_eq!(encode(b"hi"), "6869");
  }

  #[test]
  fn lower_case_only() {
    let s = encode(&[0xde, 0xad, 0xbe, 0xef]);
    assert_eq!(s, "deadbeef");
    assert!(s.chars().all(|c| !c.is_ascii_uppercase()));
  }
}
