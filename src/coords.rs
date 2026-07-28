//! Coordinate system contract.
//!
//! Three distinct coordinate worlds, kept apart by the type system:
//!
//! - **File byte offsets** (`FileByteOffset` / `FileByteRange`): offsets into
//!   the raw, unmodified bytes of one whole file.
//! - **Line byte offsets** (`LineByteOffset` / `LineByteRange`): offsets
//!   relative to the start of one line's *body* (CR/LF excluded). This is the
//!   coordinate space difft spans live in (measured against difft 0.69.0).
//! - **Display columns** (`DisplayColumn`): terminal cells, computed with
//!   unicode-width at render time only.
//!
//! The only blessed conversion between the first two is
//! [`LineByteRange::to_file`], which validates that the range fits inside the
//! line body it claims to describe.

/// Byte offset into a whole file's raw bytes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct FileByteOffset(pub usize);

/// Half-open byte range `[start, end)` into a whole file's raw bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FileByteRange {
    pub start: usize,
    pub end: usize,
}

impl FileByteRange {
    pub fn new(start: usize, end: usize) -> Self {
        debug_assert!(start <= end);
        Self { start, end }
    }

    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    pub fn contains_offset(&self, off: FileByteOffset) -> bool {
        self.start <= off.0 && off.0 < self.end
    }

    /// Slice `bytes` (the whole file) to this range.
    pub fn slice<'a>(&self, bytes: &'a [u8]) -> &'a [u8] {
        &bytes[self.start..self.end]
    }
}

/// Byte offset relative to the start of one line's body (CR/LF excluded).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct LineByteOffset(pub usize);

/// Half-open byte range `[start, end)` relative to one line's body.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct LineByteRange {
    pub start: usize,
    pub end: usize,
}

impl LineByteRange {
    pub fn new(start: usize, end: usize) -> Self {
        debug_assert!(start <= end);
        Self { start, end }
    }

    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Convert to file coordinates, given the body range of the line this
    /// range is relative to. Returns `None` if the range does not fit inside
    /// the body. This is the only sanctioned line→file conversion.
    pub fn to_file(&self, body: FileByteRange) -> Option<FileByteRange> {
        if self.start > self.end || self.end > body.len() {
            return None;
        }
        Some(FileByteRange::new(
            body.start + self.start,
            body.start + self.end,
        ))
    }
}

/// 0-based line index. Add 1 only at display time.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct LineIndex(pub usize);

/// Terminal display column. Only produced at render time via unicode-width.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DisplayColumn(pub u16);

/// Tab width used everywhere a tab must be expanded for display.
pub const TAB_WIDTH: u16 = 4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_range_to_file_within_body() {
        let body = FileByteRange::new(10, 20);
        let r = LineByteRange::new(2, 5);
        assert_eq!(r.to_file(body), Some(FileByteRange::new(12, 15)));
    }

    #[test]
    fn line_range_to_file_at_body_end_is_ok() {
        let body = FileByteRange::new(10, 20);
        let r = LineByteRange::new(0, 10);
        assert_eq!(r.to_file(body), Some(FileByteRange::new(10, 20)));
    }

    #[test]
    fn line_range_to_file_out_of_bounds_rejected() {
        let body = FileByteRange::new(10, 20);
        let r = LineByteRange::new(5, 11);
        assert_eq!(r.to_file(body), None);
    }
}
