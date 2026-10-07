//! The owner record, bytes 4096 to 8191 of page 0 (spec/08 section 8.12).

use rupg_common::{Error, Hlc, Result, SqlState};

use crate::le::{
    block_is_sealed, get, put, put_u16, put_u32, put_u64, seal_block, u16_at, u32_at, u64_at,
};
use crate::text::{TextIn, TextOut, hex_bytes};
use crate::{Block, FileId};

/// The magic bytes at offset 0 of the record.
pub const OWNER_MAGIC: [u8; 8] = *b"RUPGOWNR";

/// The byte offset of the record in page 0. The owner lock covers the same 4096 bytes.
pub const OWNER_OFFSET: usize = 4096;

/// The largest socket address in bytes. A Unix socket path is at most 104 bytes on macOS, and the field is larger so that a Windows pipe name fits.
pub const SOCKET_MAX: usize = 256;

/// The owner record. It says how another process reaches the owner of the file. It is advisory, and it is the one write in place in the file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Owner {
    /// The process id of the owner.
    pub pid: u32,
    /// The version of the protocol between the owner and the other processes (spec/17).
    pub protocol: u32,
    /// The time when the owner started.
    pub started: Hlc,
    /// The file id. A record with a different file id is stale.
    pub file_id: FileId,
    /// A hash of the machine id and the boot id. A record from a different host or boot is stale.
    pub host: [u8; 32],
    /// A Unix socket path or a Windows named pipe name.
    pub socket: String,
}

impl Owner {
    /// Writes the record with its checksum. A socket address longer than [`SOCKET_MAX`] bytes gives SQLSTATE `22023`.
    pub fn encode(&self, block: &mut Block) -> Result<()> {
        let socket = self.socket.as_bytes();
        let Ok(len) = u16::try_from(socket.len()) else { return Err(too_long(socket.len())) };
        if socket.len() > SOCKET_MAX {
            return Err(too_long(socket.len()));
        }
        block.fill(0);
        put(block, 0, &OWNER_MAGIC);
        put_u32(block, 8, self.pid);
        put_u32(block, 12, self.protocol);
        put_u64(block, 16, self.started.bits());
        put(block, 24, &self.file_id);
        put(block, 40, &self.host);
        put_u16(block, 72, len);
        put(block, 74, socket);
        seal_block(block);
        Ok(())
    }

    /// Reads and checks the record. A record that was never written, a torn record and a bad socket address give SQLSTATE `XX001`. The caller checks the file id and the host.
    pub fn decode(block: &Block) -> Result<Owner> {
        if get::<8>(block, 0) != OWNER_MAGIC {
            return Err(Error::corrupted("the owner record has a bad magic"));
        }
        if !block_is_sealed(block) {
            return Err(Error::corrupted("the owner record has a bad checksum"));
        }
        let len = usize::from(u16_at(block, 72));
        if len > SOCKET_MAX {
            return Err(Error::corrupted(format!(
                "the owner record has a socket address of {len} bytes"
            )));
        }
        let Ok(socket) = String::from_utf8(block[74..74 + len].to_vec()) else {
            return Err(Error::corrupted("the socket address in the owner record is not UTF-8"));
        };
        Ok(Owner {
            pid: u32_at(block, 8),
            protocol: u32_at(block, 12),
            started: Hlc::from_bits(u64_at(block, 16)),
            file_id: get(block, 24),
            host: get(block, 40),
            socket,
        })
    }

    /// The text form of spec/08 section 8.14.
    pub fn to_text(&self) -> String {
        let mut out = TextOut::new();
        out.field("file_id", hex_bytes(&self.file_id))
            .bytes("host", &self.host)
            .field("pid", self.pid)
            .field("protocol", self.protocol)
            .field("socket", &self.socket)
            .field("started", self.started);
        out.finish()
    }

    /// Reads the text form back.
    pub fn from_text(text: &str) -> Result<Owner> {
        let mut input = TextIn::parse("owner record", text)?;
        let owner = Owner {
            pid: input.take("pid")?,
            protocol: input.take("protocol")?,
            started: input.take("started")?,
            file_id: input.take_bytes("file_id")?,
            host: input.take_bytes("host")?,
            socket: input.take_str("socket")?.to_string(),
        };
        input.finish()?;
        Ok(owner)
    }
}

fn too_long(len: usize) -> Error {
    Error::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!(
            "the socket address has {len} bytes, and the owner record holds at most {SOCKET_MAX}"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Owner {
        Owner {
            pid: 4242,
            protocol: 1,
            started: Hlc::new(77, 0).unwrap(),
            file_id: [3; 16],
            host: [0xab; 32],
            socket: "/tmp/rupg-4242.sock".to_string(),
        }
    }

    #[test]
    fn round_trip() {
        let owner = sample();
        let mut block = [0u8; 4096];
        owner.encode(&mut block).unwrap();
        assert_eq!(&block[..8], b"RUPGOWNR");
        assert_eq!(u16_at(&block, 72), 19);
        assert_eq!(Owner::decode(&block).unwrap(), owner);
        let text = owner.to_text();
        assert!(text.starts_with("file_id 03030303030303030303030303030303\nhost abab"), "{text}");
        assert!(text.ends_with("socket /tmp/rupg-4242.sock\nstarted 77.0\n"), "{text}");
        assert_eq!(Owner::from_text(&text).unwrap(), owner);
    }

    #[test]
    fn checks() {
        let mut block = [0u8; 4096];
        assert!(Owner::decode(&block).is_err());
        let mut owner = sample();
        owner.socket = "p".repeat(SOCKET_MAX);
        owner.encode(&mut block).unwrap();
        assert_eq!(Owner::decode(&block).unwrap().socket.len(), SOCKET_MAX);
        owner.socket.push('p');
        assert_eq!(
            owner.encode(&mut block).unwrap_err().state(),
            SqlState::INVALID_PARAMETER_VALUE
        );
        owner.socket = "x".repeat(70_000);
        assert_eq!(
            owner.encode(&mut block).unwrap_err().state(),
            SqlState::INVALID_PARAMETER_VALUE
        );

        sample().encode(&mut block).unwrap();
        // A length past the field, with a correct checksum.
        put_u16(&mut block, 72, 300);
        seal_block(&mut block);
        assert_eq!(Owner::decode(&block).unwrap_err().state(), SqlState::DATA_CORRUPTED);
        // Bytes that are not UTF-8.
        put_u16(&mut block, 72, 1);
        block[74] = 0xff;
        seal_block(&mut block);
        assert_eq!(Owner::decode(&block).unwrap_err().state(), SqlState::DATA_CORRUPTED);
        // A torn write.
        block[20] ^= 1;
        assert_eq!(
            Owner::decode(&block).unwrap_err().message(),
            "the owner record has a bad checksum"
        );
    }
}
