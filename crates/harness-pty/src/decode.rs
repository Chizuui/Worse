//! Streaming UTF-8 decoding (node-pty `encoding: "utf8"` uses a
//! `StringDecoder`, so a code point split across reads is held back, and
//! invalid bytes become U+FFFD).

/// Incremental decoder holding at most 3 bytes of an incomplete code point.
#[derive(Debug, Default)]
pub struct Utf8Stream {
    carry: Vec<u8>,
}

impl Utf8Stream {
    /// Decodes `input`, prefixed with any carried bytes.
    pub fn push(&mut self, input: &[u8]) -> String {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(input);
        let mut out = String::with_capacity(buf.len());
        let mut rest: &[u8] = &buf;
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    out.push_str(text);
                    break;
                }
                Err(error) => {
                    let (valid, after) = rest.split_at(error.valid_up_to());
                    out.push_str(&String::from_utf8_lossy(valid));
                    if let Some(len) = error.error_len() {
                        out.push('\u{FFFD}');
                        rest = &after[len..];
                    } else {
                        self.carry = after.to_vec();
                        break;
                    }
                }
            }
        }
        out
    }

    /// Flushes a trailing incomplete sequence as U+FFFD (stream end).
    pub fn finish(&mut self) -> String {
        if self.carry.is_empty() {
            String::new()
        } else {
            self.carry.clear();
            "\u{FFFD}".to_owned()
        }
    }
}
