//! A small binary encoding for what passes between ferge and its preview
//! worker (see [`super::worker`]): little-endian integers, and strings and
//! byte strings with a length in front.
//!
//! The worker is ferge itself, but decoding still checks every length
//! against what is left, so a worker that died halfway or got confused
//! produces an error, never a panic or a huge allocation.

use super::DocumentError;

/// Builds a message.
#[derive(Default)]
pub(crate) struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.bytes
    }

    pub(crate) fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub(crate) fn bool(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn f32(&mut self, value: f32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    /// A count or length. Everything sent is far below 4 GiB per item.
    pub(crate) fn len(&mut self, value: usize) {
        self.u32(u32::try_from(value).unwrap_or(u32::MAX));
    }

    pub(crate) fn bytes(&mut self, value: &[u8]) {
        self.len(value.len());
        self.bytes.extend_from_slice(value);
    }

    pub(crate) fn str(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }
}

/// Reads a message.
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], DocumentError> {
        if self.bytes.len() < count {
            return Err(DocumentError::Damaged);
        }
        let (taken, rest) = self.bytes.split_at(count);
        self.bytes = rest;
        Ok(taken)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DocumentError> {
        self.take(N)?.try_into().map_err(|_| DocumentError::Damaged)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, DocumentError> {
        Ok(self.array::<1>()?[0])
    }

    pub(crate) fn bool(&mut self) -> Result<bool, DocumentError> {
        Ok(self.u8()? != 0)
    }

    pub(crate) fn u32(&mut self) -> Result<u32, DocumentError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub(crate) fn f32(&mut self) -> Result<f32, DocumentError> {
        Ok(f32::from_le_bytes(self.array()?))
    }

    pub(crate) fn len(&mut self) -> Result<usize, DocumentError> {
        usize::try_from(self.u32()?).map_err(|_| DocumentError::Damaged)
    }

    /// A count of items that take at least `item_size` bytes each, checked
    /// against what is left so that a bad count cannot make the caller
    /// reserve a huge vector.
    pub(crate) fn count(&mut self, item_size: usize) -> Result<usize, DocumentError> {
        let count = self.len()?;
        if count.saturating_mul(item_size.max(1)) > self.bytes.len() {
            return Err(DocumentError::Damaged);
        }
        Ok(count)
    }

    pub(crate) fn bytes(&mut self) -> Result<&'a [u8], DocumentError> {
        let length = self.len()?;
        self.take(length)
    }

    pub(crate) fn string(&mut self) -> Result<String, DocumentError> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| DocumentError::Damaged)
    }

    /// Fails unless everything was read.
    pub(crate) fn end(&self) -> Result<(), DocumentError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(DocumentError::Damaged)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_rejects_bad_lengths() {
        let mut writer = Writer::new();
        writer.u8(7);
        writer.bool(true);
        writer.u32(0xdead_beef);
        writer.f32(1.5);
        writer.str("grüße");
        writer.bytes(&[1, 2, 3]);
        let bytes = writer.finish();

        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.u8(), Ok(7));
        assert_eq!(reader.bool(), Ok(true));
        assert_eq!(reader.u32(), Ok(0xdead_beef));
        assert_eq!(reader.f32(), Ok(1.5));
        assert_eq!(reader.string().as_deref(), Ok("grüße"));
        assert_eq!(reader.bytes(), Ok(&[1_u8, 2, 3][..]));
        assert_eq!(reader.end(), Ok(()));

        // A length past the end, a count that cannot fit, invalid UTF-8.
        assert!(Reader::new(&[9, 0, 0, 0, 1]).bytes().is_err());
        assert!(Reader::new(&[0xff, 0xff, 0xff, 0xff]).count(1).is_err());
        assert!(Reader::new(&[1, 0, 0, 0, 0xff]).string().is_err());
        assert!(Reader::new(&[1]).end().is_err());
    }
}
