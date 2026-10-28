//! `LZ4_compress_default` of the LZ4 library, which PostgreSQL calls in `lz4_compress_datum`. The output is the same, byte for byte, so the length of a compressed TOAST value is the same as in PostgreSQL.

const MINMATCH: usize = 4;
const MFLIMIT: usize = 12;
const LASTLITERALS: usize = 5;
const MIN_LENGTH: usize = MFLIMIT + 1;
const HASHLOG: u32 = 12;
const SKIP_TRIGGER: u32 = 6;
const ML_MASK: u32 = 15;
const RUN_MASK: usize = 15;
const DISTANCE_MAX: usize = 65535;
/// `LZ4_64Klimit`: a shorter input uses the table of 16-bit positions.
const LIMIT_64K: usize = 65536 + MFLIMIT - 1;

/// `LZ4_compressBound`.
#[must_use]
pub fn bound(len: usize) -> usize {
    len + len / 255 + 16
}

/// `LZ4_compress_default` with the output buffer of `LZ4_compressBound`: the compressed block.
#[must_use]
pub fn compress(src: &[u8]) -> Vec<u8> {
    let small = src.len() < LIMIT_64K;
    let mut table = vec![0u32; if small { 1 << (HASHLOG + 1) } else { 1 << HASHLOG }];
    let mut out = Vec::with_capacity(bound(src.len()));
    let end = src.len();
    let mut anchor = 0;
    if end >= MIN_LENGTH {
        let mflimit_plus_one = end - MFLIMIT + 1;
        let match_limit = end - LASTLITERALS;
        let hash = |at: usize| hash_at(src, at, small);
        let mut ip = 0;
        table[hash(ip)] = 0;
        ip += 1;
        let mut forward_h = hash(ip);
        'outer: loop {
            let mut forward = ip;
            let mut step = 1;
            let mut search = 1u32 << SKIP_TRIGGER;
            let mut found;
            loop {
                let h = forward_h;
                let current = forward;
                let candidate = table[h] as usize;
                ip = forward;
                forward += step;
                step = (search >> SKIP_TRIGGER) as usize;
                search += 1;
                if forward > mflimit_plus_one {
                    break 'outer;
                }
                found = candidate;
                forward_h = hash(forward);
                table[h] = u32::try_from(current).unwrap_or(u32::MAX);
                if !small && candidate + DISTANCE_MAX < current {
                    continue;
                }
                if read32(src, found) == read32(src, ip) {
                    break;
                }
            }
            while ip > anchor && found > 0 && src[ip - 1] == src[found - 1] {
                ip -= 1;
                found -= 1;
            }
            let token = out.len();
            out.push(0);
            let literals = ip - anchor;
            if literals >= RUN_MASK {
                out[token] = (RUN_MASK as u8) << 4;
                push_length(&mut out, literals - RUN_MASK);
            } else {
                out[token] = (literals as u8) << 4;
            }
            out.extend_from_slice(&src[anchor..ip]);
            let mut token = token;
            loop {
                let offset = ip - found;
                out.extend_from_slice(&(offset as u16).to_le_bytes());
                let mut code = count(src, ip + MINMATCH, found + MINMATCH, match_limit) as u32;
                ip += code as usize + MINMATCH;
                if code >= ML_MASK {
                    out[token] += ML_MASK as u8;
                    code -= ML_MASK;
                    push_length(&mut out, code as usize);
                } else {
                    out[token] += code as u8;
                }
                anchor = ip;
                if ip >= mflimit_plus_one {
                    break 'outer;
                }
                let at = ip - 2;
                table[hash(at)] = u32::try_from(at).unwrap_or(u32::MAX);
                let h = hash(ip);
                let candidate = table[h] as usize;
                table[h] = u32::try_from(ip).unwrap_or(u32::MAX);
                if (small || candidate + DISTANCE_MAX >= ip)
                    && read32(src, candidate) == read32(src, ip)
                {
                    token = out.len();
                    out.push(0);
                    found = candidate;
                    continue;
                }
                break;
            }
            ip += 1;
            forward_h = hash(ip);
        }
    }
    let last = end - anchor;
    if last >= RUN_MASK {
        out.push((RUN_MASK as u8) << 4);
        push_length(&mut out, last - RUN_MASK);
    } else {
        out.push((last as u8) << 4);
    }
    out.extend_from_slice(&src[anchor..]);
    out
}

/// A length of 15 or more in the token continues in bytes of 255 and a last byte of less than 255.
fn push_length(out: &mut Vec<u8>, mut len: usize) {
    while len >= 255 {
        out.push(255);
        len -= 255;
    }
    out.push(len as u8);
}

fn read32(src: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([src[at], src[at + 1], src[at + 2], src[at + 3]])
}

/// `LZ4_hashPosition`: `LZ4_hash4` for the table of 16-bit positions, and `LZ4_hash5` of 64-bit platforms for the table of 32-bit positions.
fn hash_at(src: &[u8], at: usize, small: bool) -> usize {
    if small {
        (read32(src, at).wrapping_mul(2_654_435_761) >> (32 - (HASHLOG + 1))) as usize
    } else {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&src[at..at + 8]);
        let sequence = u64::from_le_bytes(bytes);
        ((sequence << 24).wrapping_mul(889_523_592_379) >> (64 - HASHLOG)) as usize
    }
}

/// `LZ4_count`: the number of bytes that are the same at `ip` and at `found`, up to `limit`.
fn count(src: &[u8], ip: usize, found: usize, limit: usize) -> usize {
    if ip >= limit {
        return 0;
    }
    src[ip..limit].iter().zip(&src[found..]).take_while(|(a, b)| a == b).count()
}

/// `LZ4_decompress_safe`, for the tests.
#[cfg(test)]
pub(crate) fn decompress(block: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut at = 0;
    let more = |block: &[u8], at: &mut usize, mut len: usize| {
        loop {
            let byte = block[*at];
            *at += 1;
            len += usize::from(byte);
            if byte != 255 {
                return len;
            }
        }
    };
    while at < block.len() {
        let token = block[at];
        at += 1;
        let mut literals = usize::from(token >> 4);
        if literals == RUN_MASK {
            literals = more(block, &mut at, literals);
        }
        out.extend_from_slice(&block[at..at + literals]);
        at += literals;
        if at == block.len() {
            break;
        }
        let offset = usize::from(u16::from_le_bytes([block[at], block[at + 1]]));
        at += 2;
        let mut len = usize::from(token & 15);
        if len == 15 {
            len = more(block, &mut at, len);
        }
        for _ in 0..len + MINMATCH {
            out.push(out[out.len() - offset]);
        }
    }
    out
}
