//! The identity block, bytes 0 to 4095 of page 0 (spec/08 section 8.2.1).

use rupg_common::{Error, Hlc, Result, SqlState};

use crate::le::{
    block_is_sealed, get, put, put_text, put_u16, put_u64, seal_block, text_at, u8_at, u16_at,
    u64_at,
};
use crate::text::{TextIn, TextOut, hex_bytes};
use crate::{Block, CHECKSUM_ALGORITHM, Features, FileId, PAGE_SHIFT};

/// The magic bytes at offset 0, `\x89RUPG\r\n\x1a`. They follow the PNG signature, so a transfer in text mode changes them.
pub const MAGIC: [u8; 8] = *b"\x89RUPG\r\n\x1a";

/// The format major number that this version writes. It is 0 until the first release.
pub const FORMAT_MAJOR: u16 = 0;

/// The format minor number that this version writes.
pub const FORMAT_MINOR: u16 = 0;

const CREATOR_LEN: usize = 32;
const ENCRYPTION_LEN: usize = 128;

/// The identity block. It lets a tool recognise the file and read the page size before it reads a slot. The header slots repeat its important fields, and the slots are authoritative.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The format major number.
    pub format_major: u16,
    /// The format minor number.
    pub format_minor: u16,
    /// The feature flags.
    pub features: Features,
    /// The file id.
    pub file_id: FileId,
    /// The time of the create.
    pub created: Hlc,
    /// The version of rupg that made the file, for example `rupg 0.1.0`. It is cut to 32 bytes.
    pub creator: String,
    /// The encryption parameters of spec/08 section 8.11, zero when the file is not encrypted.
    pub encryption: [u8; ENCRYPTION_LEN],
}

impl Identity {
    /// The identity block of a new file with the current format and no features.
    pub fn new(file_id: FileId, created: Hlc, creator: &str) -> Identity {
        Identity {
            format_major: FORMAT_MAJOR,
            format_minor: FORMAT_MINOR,
            features: Features::NONE,
            file_id,
            created,
            creator: creator.to_string(),
            encryption: [0; ENCRYPTION_LEN],
        }
    }

    /// Writes the block with its checksum.
    pub fn encode(&self, block: &mut Block) {
        block.fill(0);
        put(block, 0, &MAGIC);
        put_u16(block, 8, self.format_major);
        put_u16(block, 10, self.format_minor);
        block[12] = PAGE_SHIFT;
        block[13] = CHECKSUM_ALGORITHM;
        put_u64(block, 16, self.features.compat);
        put_u64(block, 24, self.features.ro_compat);
        put_u64(block, 32, self.features.incompat);
        put(block, 40, &self.file_id);
        put_u64(block, 56, self.created.bits());
        put_text(block, 64, CREATOR_LEN, &self.creator);
        put(block, 96, &self.encryption);
        seal_block(block);
    }

    /// Reads and checks the block. The checks are in this order: the magic, the checksum, the format version, the page size and the checksum algorithm. A bad magic or a bad checksum gives SQLSTATE `XX001`. A version, a page size or an algorithm that this rupg does not read gives `0A000`. The feature flags are not checked here, see [`Features::access`].
    pub fn decode(block: &Block) -> Result<Identity> {
        let magic: [u8; 8] = get(block, 0);
        if magic != MAGIC {
            return Err(Error::corrupted("the file is not a rupg file: the magic bytes are wrong")
                .with_detail(format!("The first 8 bytes are {}.", hex_bytes(&magic)))
                .with_hint(
                    "A copy in text mode changes these bytes. Copy the file in binary mode.",
                ));
        }
        if !block_is_sealed(block) {
            return Err(Error::corrupted("the identity block of the file has a bad checksum")
                .with_hint(
                    "The header slots repeat the identity. Run rupg inspect --salvage on the file.",
                ));
        }
        let format_major = u16_at(block, 8);
        let format_minor = u16_at(block, 10);
        if format_major != FORMAT_MAJOR || format_minor > FORMAT_MINOR {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!(
                    "the file has format {format_major}.{format_minor}, and this rupg reads format {FORMAT_MAJOR}.0 to {FORMAT_MAJOR}.{FORMAT_MINOR}"
                ),
            )
            .with_hint("Use rupg-cli copy to convert the file."));
        }
        let shift = u8_at(block, 12);
        if shift != PAGE_SHIFT {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!(
                    "the file has a page size shift of {shift}, and this rupg reads only {PAGE_SHIFT}"
                ),
            ));
        }
        let algorithm = u8_at(block, 13);
        if algorithm != CHECKSUM_ALGORITHM {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!(
                    "the file uses checksum algorithm {algorithm}, and this rupg knows only algorithm {CHECKSUM_ALGORITHM}"
                ),
            ));
        }
        Ok(Identity {
            format_major,
            format_minor,
            features: Features {
                compat: u64_at(block, 16),
                ro_compat: u64_at(block, 24),
                incompat: u64_at(block, 32),
            },
            file_id: get(block, 40),
            created: Hlc::from_bits(u64_at(block, 56)),
            creator: text_at(block, 64, CREATOR_LEN),
            encryption: get(block, 96),
        })
    }

    /// The text form of spec/08 section 8.14.
    pub fn to_text(&self) -> String {
        let mut out = TextOut::new();
        out.field("created", self.created)
            .field("creator", &self.creator)
            .bytes("encryption", &self.encryption)
            .field("file_id", hex_bytes(&self.file_id))
            .field("format", format_args!("{}.{}", self.format_major, self.format_minor));
        self.features.put(&mut out);
        out.finish()
    }

    /// Reads the text form back.
    pub fn from_text(text: &str) -> Result<Identity> {
        let mut input = TextIn::parse("identity block", text)?;
        let format = input.take_str("format")?;
        let version = format
            .split_once('.')
            .and_then(|(major, minor)| Some((major.parse().ok()?, minor.parse().ok()?)));
        let Some((format_major, format_minor)) = version else {
            return Err(Error::new(
                SqlState::INVALID_TEXT_REPRESENTATION,
                format!(
                    "bad text form of the identity block: the format \"{format}\" is not major.minor"
                ),
            ));
        };
        let identity = Identity {
            format_major,
            format_minor,
            features: Features::take(&mut input)?,
            file_id: input.take_bytes("file_id")?,
            created: input.take("created")?,
            creator: input.take_str("creator")?.to_string(),
            encryption: input.take_bytes("encryption")?,
        };
        input.finish()?;
        Ok(identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Identity {
        let mut id = Identity::new([7; 16], Hlc::new(1000, 2).unwrap(), "rupg 0.0.1");
        id.features.compat = 4;
        id
    }

    #[test]
    fn round_trip() {
        let id = sample();
        let mut block = [0u8; 4096];
        id.encode(&mut block);
        assert_eq!(&block[..8], b"\x89RUPG\r\n\x1a");
        assert_eq!(block[12], 14);
        assert_eq!(block[13], 1);
        assert_eq!(Identity::decode(&block).unwrap(), id);
        let text = id.to_text();
        let expected = [
            "created 1000.2",
            "creator rupg 0.0.1",
            "encryption none",
            "features_compat 0x0000000000000004",
            "features_incompat 0x0000000000000000",
            "features_ro_compat 0x0000000000000000",
            "file_id 07070707070707070707070707070707",
            "format 0.0",
        ];
        assert_eq!(text.lines().collect::<Vec<_>>(), expected);
        assert_eq!(Identity::from_text(&text).unwrap(), id);
        assert!(Identity::from_text(&text.replace("format 0.0", "format 0")).is_err());
    }

    #[test]
    fn checks() {
        let mut block = [0u8; 4096];
        sample().encode(&mut block);
        let state = |b: &Block| Identity::decode(b).unwrap_err().state();

        // A transfer that strips the high bit of each byte.
        let mut stripped = block;
        stripped[0] &= 0x7f;
        assert_eq!(state(&stripped), SqlState::DATA_CORRUPTED);
        let e = Identity::decode(&stripped).unwrap_err();
        assert!(e.message().contains("magic"));

        let mut flipped = block;
        flipped[3000] ^= 1;
        assert_eq!(state(&flipped), SqlState::DATA_CORRUPTED);

        let mut newer = sample();
        newer.format_major = 1;
        newer.encode(&mut block);
        assert_eq!(state(&block), SqlState::FEATURE_NOT_SUPPORTED);
        newer.format_major = 0;
        newer.format_minor = FORMAT_MINOR + 1;
        newer.encode(&mut block);
        assert_eq!(state(&block), SqlState::FEATURE_NOT_SUPPORTED);

        sample().encode(&mut block);
        block[12] = 13;
        seal_block(&mut block);
        assert_eq!(state(&block), SqlState::FEATURE_NOT_SUPPORTED);
        block[12] = 14;
        block[13] = 2;
        seal_block(&mut block);
        assert_eq!(state(&block), SqlState::FEATURE_NOT_SUPPORTED);
    }

    /// A creator name longer than 32 bytes is cut at a character boundary.
    #[test]
    fn long_creator() {
        let id = Identity::new([1; 16], Hlc::ZERO, &"é".repeat(20));
        let mut block = [0u8; 4096];
        id.encode(&mut block);
        assert_eq!(Identity::decode(&block).unwrap().creator, "é".repeat(16));
    }
}
