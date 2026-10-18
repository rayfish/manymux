//! Turning bytes that arrive in arbitrary chunks into text.
//!
//! Built without the `desktop` feature, because both ends need it: the node
//! feeding a session's PTY output to its screen model, and a mobile client
//! feeding what arrives off the attach stream to its own.

use std::str::from_utf8;

/// Incremental UTF-8 decoder.
///
/// `avt` takes `&str`, but PTY output arrives in arbitrary chunks that split
/// multi-byte characters. This holds the incomplete tail until the rest shows
/// up, and substitutes U+FFFD for genuinely invalid bytes so one bad byte can't
/// desynchronise the screen forever.
#[derive(Debug, Default)]
pub struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    pub fn decode(&mut self, chunk: &[u8]) -> String {
        let bytes: &[u8] = if self.pending.is_empty() {
            chunk
        } else {
            self.pending.extend_from_slice(chunk);
            &self.pending
        };

        let mut out = String::with_capacity(bytes.len());
        let mut rest = bytes;
        loop {
            match from_utf8(rest) {
                Ok(s) => {
                    out.push_str(s);
                    rest = &[];
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    // Valid up to here by the error's own account, so the
                    // fallback is never taken.
                    out.push_str(from_utf8(&rest[..valid]).unwrap_or_default());
                    match e.error_len() {
                        // Invalid bytes: emit a replacement and skip them.
                        Some(n) => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            rest = &rest[valid + n..];
                        }
                        // Truncated at the end of the chunk: keep it for later.
                        None => {
                            rest = &rest[valid..];
                            break;
                        }
                    }
                }
            }
        }

        let tail = rest.to_vec();
        self.pending = tail;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_split_across_chunks() {
        let mut d = Utf8Decoder::default();
        let s = "héllo ✓";
        let bytes = s.as_bytes();
        let mut out = String::new();
        // Feed one byte at a time, the worst case for multi-byte characters.
        for b in bytes {
            out.push_str(&d.decode(&[*b]));
        }
        assert_eq!(out, s);
    }

    #[test]
    fn invalid_utf8_becomes_replacement() {
        let mut d = Utf8Decoder::default();
        assert_eq!(d.decode(b"a\xffb"), "a\u{fffd}b");
    }
}
