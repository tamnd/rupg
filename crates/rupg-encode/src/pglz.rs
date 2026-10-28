//! `pglz_compress` of PostgreSQL with `PGLZ_strategy_default`, which `pglz_compress_datum` calls. The output is the same, byte for byte, so the length of a compressed TOAST value is the same as in PostgreSQL.

const MAX_HISTORY_LISTS: usize = 8192;
const HISTORY_SIZE: usize = 4096;
const MAX_MATCH: usize = 273;
/// `PGLZ_strategy_default`: the shortest input that the compressor takes.
pub const MIN_INPUT_SIZE: usize = 32;
const MIN_COMP_RATE: usize = 25;
const FIRST_SUCCESS_BY: usize = 1024;
const MATCH_SIZE_GOOD: usize = 128;
const MATCH_SIZE_DROP: usize = 10;

/// `PGLZ_HistEntry`, with indexes in place of pointers. Entry 0 is `INVALID_ENTRY`.
#[derive(Clone, Copy, Default)]
struct Entry {
    next: usize,
    prev: Option<usize>,
    index: usize,
    pos: usize,
}

/// `pglz_compress` with `PGLZ_strategy_default`: the compressed data, or `None` when the input is too short or too long, or when the data does not get at least 25% shorter.
#[must_use]
pub fn compress(src: &[u8]) -> Option<Vec<u8>> {
    let len = src.len();
    if len < MIN_INPUT_SIZE || len > i32::MAX as usize {
        return None;
    }
    let result_max = if len > i32::MAX as usize / 100 {
        (len / 100) * (100 - MIN_COMP_RATE)
    } else {
        (len * (100 - MIN_COMP_RATE)) / 100
    };
    let hash_size = match len {
        0..128 => 512,
        128..256 => 1024,
        256..512 => 2048,
        512..1024 => 4096,
        _ => MAX_HISTORY_LISTS,
    };
    let mask = hash_size - 1;
    let mut start = vec![0usize; hash_size];
    let mut entries = vec![Entry::default(); HISTORY_SIZE + 1];
    let mut next = 1;
    let mut recycle = false;
    let mut out: Vec<u8> = Vec::with_capacity(result_max + 4);
    // The control byte goes at `ctrl_at`, which is `None` before the first control byte.
    let mut ctrl_at: Option<usize> = None;
    let mut ctrl_bits = 0u8;
    let mut ctrl = 0u8;
    let mut found_match = false;
    let mut dp = 0;
    let mut add = |entries: &mut Vec<Entry>, start: &mut Vec<usize>, at: usize| {
        let index = hist_index(src, at, mask);
        let me = next;
        if recycle {
            let old = entries[me];
            match old.prev {
                None => start[old.index] = old.next,
                Some(prev) => entries[prev].next = old.next,
            }
            entries[old.next].prev = old.prev;
        }
        let head = start[index];
        entries[me] = Entry { next: head, prev: None, index, pos: at };
        if head != 0 {
            entries[head].prev = Some(me);
        }
        start[index] = me;
        next += 1;
        if next > HISTORY_SIZE {
            next = 1;
            recycle = true;
        }
    };
    let out_ctrl =
        |out: &mut Vec<u8>, ctrl_at: &mut Option<usize>, bits: &mut u8, ctrl: &mut u8| {
            if *ctrl == 0 {
                if let Some(at) = *ctrl_at {
                    out[at] = *bits;
                }
                *ctrl_at = Some(out.len());
                out.push(0);
                *bits = 0;
                *ctrl = 1;
            }
        };
    while dp < len {
        if out.len() >= result_max {
            return None;
        }
        if !found_match && out.len() >= FIRST_SUCCESS_BY {
            return None;
        }
        if let Some((match_len, off)) = find_match(&entries, &start, src, dp, mask) {
            out_ctrl(&mut out, &mut ctrl_at, &mut ctrl_bits, &mut ctrl);
            ctrl_bits |= ctrl;
            ctrl <<= 1;
            if match_len > 17 {
                out.push((((off & 0xf00) >> 4) | 0x0f) as u8);
                out.push((off & 0xff) as u8);
                out.push((match_len - 18) as u8);
            } else {
                out.push((((off & 0xf00) >> 4) | (match_len - 3)) as u8);
                out.push((off & 0xff) as u8);
            }
            for _ in 0..match_len {
                add(&mut entries, &mut start, dp);
                dp += 1;
            }
            found_match = true;
        } else {
            out_ctrl(&mut out, &mut ctrl_at, &mut ctrl_bits, &mut ctrl);
            out.push(src[dp]);
            ctrl <<= 1;
            add(&mut entries, &mut start, dp);
            dp += 1;
        }
    }
    if let Some(at) = ctrl_at {
        out[at] = ctrl_bits;
    }
    if out.len() >= result_max {
        return None;
    }
    Some(out)
}

/// `pglz_hist_idx`. The bytes are `char`, which is signed on the platforms that PostgreSQL builds for in the usual way.
fn hist_index(src: &[u8], at: usize, mask: usize) -> usize {
    let byte = |i: usize| i32::from(src[at + i] as i8);
    let value = if src.len() - at < 4 {
        byte(0)
    } else {
        (byte(0) << 6) ^ (byte(1) << 4) ^ (byte(2) << 2) ^ byte(3)
    };
    (value as usize) & mask
}

/// `pglz_find_match`: the length and the offset of the best match at `at`, when it is longer than 2 bytes.
fn find_match(
    entries: &[Entry],
    start: &[usize],
    src: &[u8],
    at: usize,
    mask: usize,
) -> Option<(usize, usize)> {
    let end = src.len();
    let mut good_match = MATCH_SIZE_GOOD;
    let mut len = 0;
    let mut off = 0;
    let mut entry = start[hist_index(src, at, mask)];
    while entry != 0 {
        let hp = entries[entry].pos;
        let this_off = at - hp;
        if this_off >= 0x0fff {
            break;
        }
        let mut this_len = 0;
        if len >= 16 {
            if src[at..at + len] == src[hp..hp + len] {
                this_len = len;
                while at + this_len < end
                    && src[at + this_len] == src[hp + this_len]
                    && this_len < MAX_MATCH
                {
                    this_len += 1;
                }
            }
        } else {
            while at + this_len < end
                && src[at + this_len] == src[hp + this_len]
                && this_len < MAX_MATCH
            {
                this_len += 1;
            }
        }
        if this_len > len {
            len = this_len;
            off = this_off;
        }
        entry = entries[entry].next;
        if entry != 0 {
            if len >= good_match {
                break;
            }
            good_match -= (good_match * MATCH_SIZE_DROP) / 100;
        }
    }
    (len > 2).then_some((len, off))
}

/// `pglz_decompress`, for the tests.
#[cfg(test)]
pub(crate) fn decompress(block: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut at = 0;
    while at < block.len() {
        let ctrl = block[at];
        at += 1;
        for bit in 0..8 {
            if at >= block.len() {
                break;
            }
            if ctrl & (1 << bit) != 0 {
                let mut len = usize::from(block[at] & 0x0f) + 3;
                let off = (usize::from(block[at] & 0xf0) << 4) | usize::from(block[at + 1]);
                at += 2;
                if len == 18 {
                    len += usize::from(block[at]);
                    at += 1;
                }
                for _ in 0..len {
                    out.push(out[out.len() - off]);
                }
            } else {
                out.push(block[at]);
                at += 1;
            }
        }
    }
    out
}
