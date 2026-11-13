// Lifted from rudb crates/rudb-qc-rt/src/abi.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit under `crate::rt`; `RT` reads the context offset from `crate::ir`, and the notes name the native tier, which is not lifted yet, in plain text.

//! The memory a pipeline function reads: the morsel, the column table it points at, and the state
//! header, per `spec/compiler/05-pipelines-and-state.md` sections 5.3 and 5.5.
//!
//! These are the offsets the generator bakes into loads, so they are constants here and the
//! structs exist to make the layout checkable, not to be read field by field from Rust.

/// One unit of work handed to a pipeline body.
///
/// The first 32 bytes are the spec's morsel. `cols` is the one addition: in the interpreter tier
/// the driver has already decoded the source chunk, so the body reads flat column buffers through
/// this table rather than asking a scan for them.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Morsel {
    /// Which source of the pipeline the rows come from.
    pub source: u32,
    /// The chunk of that source.
    pub chunk: u32,
    /// The first row, relative to the chunk.
    pub begin: u32,
    /// One past the last row.
    pub end: u32,
    /// The global order key.
    pub seq: u64,
    /// The encoding tag of the columns.
    pub enc: u32,
    /// Per morsel facts.
    pub flags: u32,
    /// The column table, one [`Col`] per source column the pipeline reads.
    pub cols: *const Col,
}

/// Where one column of a morsel is.
///
/// `values` holds the column at its physical width, with strings as `str16`. `valid` is a bitmap,
/// least significant bit first, and is never null: a column with no nulls points at a buffer of
/// ones, so the body has one way to read validity and the fast path is a later tier's business.
///
/// A text column, and each answer or index column made from one, is read through `codes`: the
/// value of row `i` is `values[codes[i]]`. A column coded into a dictionary hands its codes and a
/// table made once per dictionary, so no row is copied out, and a flat one hands `0, 1, 2, ...`
/// and a value a row. Every other column leaves `codes` null and is read at the row.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Col {
    /// The values.
    pub values: *const u8,
    /// The validity bitmap.
    pub valid: *const u8,
    /// The index into `values` of each row, for a column read through one.
    pub codes: *const u32,
}

/// The byte offset of [`Morsel::begin`].
pub const MORSEL_BEGIN: i32 = 8;
/// The byte offset of [`Morsel::end`].
pub const MORSEL_END: i32 = 12;
/// The byte offset of [`Morsel::cols`].
pub const MORSEL_COLS: i32 = 32;
/// The size of a [`Col`].
pub const COL_SIZE: i32 = 24;
/// The byte offset of [`Col::valid`].
pub const COL_VALID: i32 = 8;
/// The byte offset of [`Col::codes`].
pub const COL_CODES: i32 = 16;

/// The first cache line of every pipeline's state, before the local slots.
///
/// The body reads its fields at the offsets below. `rt` and `profile` are opaque here. The
/// interpreter tier passes the runtime to a step as an argument, so `rt` is null under it, and a
/// native tier finds the runtime through `rt` instead (see the native tier). Nothing keeps
/// counters yet, so `profile` stays null.
#[repr(C, align(64))]
#[derive(Clone, Copy, Debug)]
pub struct StateHeader {
    /// The shared state of the pipeline. With one worker the local state is the shared state.
    pub shared: *const u8,
    /// The runtime ABI.
    pub rt: *const u8,
    /// The worker's profiling counters.
    pub profile: *mut u8,
    /// Where a body that returned `Yield` or `NeedMemory` picks up again.
    pub cursor: u64,
    /// The deferred error word.
    pub error: u32,
    /// The guard site of the last deopt.
    pub deopt_site: u32,
    /// Which worker owns this state.
    pub worker: u16,
    /// Which body variant is live.
    pub variant: u16,
    /// The countdown for `poll`.
    pub poll_left: u32,
    /// The parameter block, null when the query has no parameters.
    pub params: *const u8,
    _pad: [u8; 8],
}

impl StateHeader {
    /// A header for a state block at `state` run by one worker, which is its own shared state.
    #[must_use]
    pub fn new(state: *const u8) -> StateHeader {
        StateHeader {
            shared: state,
            rt: std::ptr::null(),
            profile: std::ptr::null_mut(),
            cursor: 0,
            error: 0,
            deopt_site: 0,
            worker: 0,
            variant: 0,
            poll_left: 0,
            params: std::ptr::null(),
            _pad: [0; 8],
        }
    }
}

/// The size of the state header. Local slots start here.
pub const HEADER: u32 = 64;
/// The header's `rt` word, which holds the context of the native tier while native code runs.
pub const RT: i32 = crate::ir::entry::CTX_OFFSET;
/// The header's `cursor` word, where a body that yields records how far it got.
pub const CURSOR: i32 = 24;
/// The header's deferred error word.
pub const ERROR: i32 = 32;
/// The header's `poll_left` countdown.
pub const POLL_LEFT: i32 = 44;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_offsets_are_the_layout() {
        assert_eq!(size_of::<Morsel>(), 40);
        assert_eq!(std::mem::offset_of!(Morsel, begin), MORSEL_BEGIN as usize);
        assert_eq!(std::mem::offset_of!(Morsel, end), MORSEL_END as usize);
        assert_eq!(std::mem::offset_of!(Morsel, cols), MORSEL_COLS as usize);
        assert_eq!(size_of::<Col>(), COL_SIZE as usize);
        assert_eq!(std::mem::offset_of!(Col, valid), COL_VALID as usize);
        assert_eq!(std::mem::offset_of!(Col, codes), COL_CODES as usize);
        assert_eq!(size_of::<StateHeader>(), HEADER as usize);
        assert_eq!(align_of::<StateHeader>(), 64);
        assert_eq!(std::mem::offset_of!(StateHeader, rt), RT as usize);
        assert_eq!(std::mem::offset_of!(StateHeader, cursor), CURSOR as usize);
        assert_eq!(std::mem::offset_of!(StateHeader, error), ERROR as usize);
        assert_eq!(std::mem::offset_of!(StateHeader, poll_left), POLL_LEFT as usize);
    }
}
