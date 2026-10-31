//! The encodings of document 09, the chooser, and the codecs: zstd, lz4, pglz and snappy.
//!
//! This crate first ships in milestone M4. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`. The `lz4` and `pglz` compressors come first, as TOAST needs them.
//!
//! The encodings are lifted from rudb at `cd9f9676` (spec/22 section 22.7). [`bitpack`] is the FastLanes layout under every integer encoding. [`integer`] and [`string`] are the encodings and their cascades. [`fsst`] is the symbol table, [`sequence`] runs `LIKE` on FSST codes, and [`sketch`] estimates the distinct count that decides whether a column gets a dictionary (spec/09 section 9.5.1). [`chooser`] picks the cascade of a chunk on a sample, and [`tally`] counts what each codec costs, for the encoder profile of Q8 (spec/24 section 24.3).

#![forbid(unsafe_code)]

pub mod bitpack;
pub mod chooser;
pub mod fsst;
pub mod integer;
mod lz;
pub mod lz4;
pub mod pglz;
mod reader;
pub mod sequence;
pub mod sketch;
pub mod string;
pub mod tally;

/// A compression method of TOAST, from `default_toast_compression` or `attcompression`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// `pglz`.
    Pglz,
    /// `lz4`.
    Lz4,
}

/// `VARHDRSZ_COMPRESSED`: the header of a compressed value, the length and then the raw length with the method.
pub const COMPRESSED_HEADER: usize = 8;

/// `toast_compress_datum`: `VARSIZE` of the compressed value, with its header of 8 bytes, or `None` when PostgreSQL keeps the value as it is. PostgreSQL keeps a value that does not get more than 2 bytes shorter.
#[must_use]
pub fn toast_compress(method: Method, data: &[u8]) -> Option<usize> {
    let len = match method {
        Method::Pglz => pglz::compress(data)?.len(),
        Method::Lz4 => {
            let len = lz4::compress(data).len();
            if len > data.len() {
                return None;
            }
            len
        }
    };
    let size = len + COMPRESSED_HEADER;
    (size + 2 < data.len()).then_some(size)
}

#[cfg(test)]
mod tests;
