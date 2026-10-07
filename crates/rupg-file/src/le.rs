//! Little endian reads and writes at fixed offsets. Each caller gives an offset inside the buffer.

pub(crate) fn get<const N: usize>(buf: &[u8], at: usize) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(&buf[at..at + N]);
    out
}

pub(crate) fn u8_at(buf: &[u8], at: usize) -> u8 {
    buf[at]
}

pub(crate) fn u16_at(buf: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(get(buf, at))
}

pub(crate) fn u32_at(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(get(buf, at))
}

pub(crate) fn u64_at(buf: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(get(buf, at))
}

pub(crate) fn put(buf: &mut [u8], at: usize, bytes: &[u8]) {
    buf[at..at + bytes.len()].copy_from_slice(bytes);
}

pub(crate) fn put_u16(buf: &mut [u8], at: usize, value: u16) {
    put(buf, at, &value.to_le_bytes());
}

pub(crate) fn put_u32(buf: &mut [u8], at: usize, value: u32) {
    put(buf, at, &value.to_le_bytes());
}

pub(crate) fn put_u64(buf: &mut [u8], at: usize, value: u64) {
    put(buf, at, &value.to_le_bytes());
}

/// Writes the checksum of the first `len - 8` bytes of a block into its last 8 bytes.
pub(crate) fn seal_block(block: &mut [u8]) {
    let end = block.len() - 8;
    let sum = crate::checksum(&block[..end]);
    put_u64(block, end, sum);
}

/// True if the last 8 bytes of a block hold the checksum of the bytes before them.
pub(crate) fn block_is_sealed(block: &[u8]) -> bool {
    let end = block.len() - 8;
    crate::checksum(&block[..end]) == u64_at(block, end)
}

/// A zero padded UTF-8 field, cut at a character boundary so that it fits.
pub(crate) fn put_text(buf: &mut [u8], at: usize, len: usize, text: &str) {
    let mut end = text.len().min(len);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    buf[at..at + len].fill(0);
    put(buf, at, &text.as_bytes()[..end]);
}

/// Reads a zero padded UTF-8 field. Bytes that are not UTF-8 are replaced.
pub(crate) fn text_at(buf: &[u8], at: usize, len: usize) -> String {
    let field = &buf[at..at + len];
    let end = field.iter().position(|&b| b == 0).unwrap_or(len);
    String::from_utf8_lossy(&field[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields() {
        let mut buf = [0u8; 64];
        put_u16(&mut buf, 0, 0x0102);
        put_u32(&mut buf, 2, 0x0304_0506);
        put_u64(&mut buf, 6, 0x0708_090a_0b0c_0d0e);
        assert_eq!(&buf[..4], &[2, 1, 6, 5]);
        assert_eq!(u16_at(&buf, 0), 0x0102);
        assert_eq!(u32_at(&buf, 2), 0x0304_0506);
        assert_eq!(u64_at(&buf, 6), 0x0708_090a_0b0c_0d0e);
        assert_eq!(u8_at(&buf, 6), 0x0e);
        // "é" is 2 bytes and does not fit in the last byte.
        put_text(&mut buf, 20, 4, "abcé");
        assert_eq!(text_at(&buf, 20, 4), "abc");
        put_text(&mut buf, 20, 4, "ab");
        assert_eq!(&buf[20..24], b"ab\0\0");
        seal_block(&mut buf);
        assert!(block_is_sealed(&buf));
        buf[3] ^= 1;
        assert!(!block_is_sealed(&buf));
    }
}
