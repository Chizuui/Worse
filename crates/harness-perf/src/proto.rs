//! Minimal protobuf wire-format encoder for the Perfetto trace schema.
//!
//! Only varints, fixed64/double and length-delimited fields are needed, and
//! nesting is at most two levels deep, so nested messages are built in a
//! scratch buffer and appended when complete instead of back-patching lengths.

const WIRE_VARINT: u8 = 0;
const WIRE_FIXED64: u8 = 1;
const WIRE_DELIMITED: u8 = 2;

/// Append-only protobuf message builder.
#[derive(Default, Debug)]
pub(crate) struct Buf {
    bytes: Vec<u8>,
}

impl Buf {
    pub(crate) fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(256),
        }
    }

    pub(crate) fn into_vec(self) -> Vec<u8> {
        self.bytes
    }

    fn varint(&mut self, mut value: u64) {
        loop {
            let byte = u8::try_from(value & 0x7f).unwrap_or(0);
            value >>= 7;
            if value == 0 {
                self.bytes.push(byte);
                return;
            }
            self.bytes.push(byte | 0x80);
        }
    }

    fn tag(&mut self, field: u32, wire: u8) {
        self.varint(u64::from(field) << 3 | u64::from(wire));
    }

    pub(crate) fn varint_field(&mut self, field: u32, value: u64) {
        self.tag(field, WIRE_VARINT);
        self.varint(value);
    }

    pub(crate) fn fixed64_field(&mut self, field: u32, value: u64) {
        self.tag(field, WIRE_FIXED64);
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn double_field(&mut self, field: u32, value: f64) {
        self.fixed64_field(field, value.to_bits());
    }

    pub(crate) fn bytes_field(&mut self, field: u32, value: &[u8]) {
        self.tag(field, WIRE_DELIMITED);
        self.varint(u64::try_from(value.len()).unwrap_or(0));
        self.bytes.extend_from_slice(value);
    }

    pub(crate) fn string_field(&mut self, field: u32, value: &str) {
        self.bytes_field(field, value.as_bytes());
    }

    pub(crate) fn message_field(&mut self, field: u32, child: Buf) {
        self.bytes_field(field, &child.into_vec());
    }
}

#[cfg(test)]
mod tests {
    use super::Buf;

    #[test]
    fn varints_use_the_smallest_encoding() {
        let mut buf = Buf::new();
        buf.varint_field(1, 0);
        buf.varint_field(1, 300);
        assert_eq!(buf.into_vec(), vec![0x08, 0x00, 0x08, 0xac, 0x02]);
    }

    #[test]
    fn delimited_field_keeps_the_payload_intact() {
        let mut inner = Buf::new();
        inner.varint_field(2, 7);
        let mut outer = Buf::new();
        outer.message_field(11, inner);
        assert_eq!(outer.into_vec(), vec![0x5a, 0x02, 0x10, 0x07]);
    }

    #[test]
    fn double_field_is_little_endian_fixed64() {
        let mut buf = Buf::new();
        buf.double_field(46, 1.0);
        let bytes = buf.into_vec();
        assert_eq!(&bytes[..2], &[0xf1, 0x02]);
        assert_eq!(&bytes[2..], &1.0_f64.to_le_bytes());
    }
}
