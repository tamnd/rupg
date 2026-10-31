//! Bit packing in the FastLanes unified transposed layout.
//!
//! Packing N values of a fixed bit width into a dense buffer is the bottom of every integer
//! encoding in `spec/06-compression.md` section 6.2. FOR subtracts a base and packs. DELTA
//! differences and packs. DICT produces codes and packs them. So this is the one kernel that runs
//! over more bytes than anything else in the system, and the layout it uses decides whether the
//! decoder can be data parallel or has to walk a dependency chain.
//!
//! The obvious layout writes value 0 in the low bits of the first word, value 1 above it, and so
//! on. Unpacking that requires knowing where the previous value ended, which is a sequential
//! dependency, and a SIMD implementation has to fight it with shuffles that differ per width and
//! per instruction set. FastLanes takes the other road. The 1024 values of a vector are seen as a
//! matrix of `T` rows by `1024 / T` lanes, where `T` is the bit width of the type, and the packing
//! runs down the rows of every lane at once. Every lane has the same bit schedule, so unpacking
//! lane 0 and lane 31 is the same instruction sequence with no cross lane data movement at all.
//! That is what makes one scalar reference implementation and one AVX-512 implementation and one
//! NEON implementation agree bit for bit, and it is why the vector size is 1024 rather than a
//! rounder number.
//!
//! The price is that the values come out permuted within the vector. Row `r` lane `l` is not the
//! `r * lanes + l`th value of the input. Section 6.2 says why that is acceptable: an operator
//! working inside one vector does not care what order the rows are in, so the permutation only
//! has to be undone when a vector is materialized in row order. [`transpose`] and [`untranspose`]
//! are that step, and they are deliberately separate from [`pack_transposed`] and
//! [`unpack_transposed`] so that the engine can keep data permuted through a whole pipeline and
//! pay for the reordering once at the end rather than twice per operator.
//!
//! There is a second layout in here, in [`pack_tail`], and it is the sequential one this module
//! opens by arguing against. The transposed layout is all or nothing: a value lives at a row and a
//! lane, the lanes are interleaved through the whole buffer, and no prefix of a packed unit holds a
//! prefix of the values. So a unit holding 3 values costs exactly what a unit holding 1024 costs,
//! and a cascade is full of short arrays. A five entry dictionary, a run length array, an exception
//! list. Storing three numbers in 5 KB is not a compressed format. The tail packer handles anything
//! shorter than a unit, it has the dependency chain the transposed layout exists to avoid, and that
//! is affordable there and nowhere else, because a tail is at most 1023 values and is decoded once
//! while a full unit is on the hot path of every scan in the system.
//!
//! The permutation itself is a fixed shuffle of the eight bit groups of a row index, in the order
//! 0, 4, 2, 6, 1, 5, 3, 7. That order is not arbitrary. It is the one that makes an eight way
//! interleave of the rows land back in sequence under the pairwise unpacking pattern the paper
//! uses, and the important property for us is only that it is a bijection that both directions
//! agree on.

// Lifted from rudb crates/rudb-encoding/src/bitpack.rs at cd9f9676 (2026-10-05).
// Changes: the error type is rupg_common::Error.

use rupg_common::{Error, Result};

/// How many values a packed unit holds. One vector, per `spec/06-compression.md` section 6.2.
pub const VALUES: usize = 1024;

/// The interleaving order of the eight row groups. See the module documentation.
const ORDER: [usize; 8] = [0, 4, 2, 6, 1, 5, 3, 7];

mod sealed {
    pub trait Sealed {}
    impl Sealed for u8 {}
    impl Sealed for u16 {}
    impl Sealed for u32 {}
    impl Sealed for u64 {}
}

/// An unsigned integer type that can be bit packed.
///
/// Sealed, because the layout constants are only correct for the four widths that divide 1024 into
/// a whole number of lanes, and because every kernel here does its arithmetic in `u64` and relies
/// on every implementor fitting in one.
pub trait Packable: sealed::Sealed + Copy + Ord + std::fmt::Debug {
    /// Width of the type in bits. `T` in the module documentation.
    const WIDTH: usize;
    /// How many of these fit in the 1024 bit virtual register, which is how many lanes there are.
    const LANES: usize = VALUES / Self::WIDTH;

    /// Widens to the type the packing arithmetic is done in.
    fn to_u64(self) -> u64;
    /// Narrows back. The high bits are already known to be zero.
    fn from_u64(value: u64) -> Self;
}

macro_rules! impl_packable {
    ($($ty:ty),*) => {$(
        impl Packable for $ty {
            const WIDTH: usize = <$ty>::BITS as usize;

            #[inline]
            fn to_u64(self) -> u64 {
                u64::from(self)
            }

            #[inline]
            fn from_u64(value: u64) -> Self {
                value as $ty
            }
        }
    )*};
}

impl_packable!(u8, u16, u32, u64);

/// A mask of the low `bits` bits, correct at 0 and at 64 where the shift would overflow.
#[inline]
const fn low_mask(bits: usize) -> u64 {
    if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 }
}

/// A right shift that saturates to zero at 64 rather than overflowing.
#[inline]
const fn shift_right(value: u64, bits: usize) -> u64 {
    if bits >= 64 { 0 } else { value >> bits }
}

/// Where the value at row `row` lane `lane` of the transposed matrix came from in the input.
///
/// The row index is split into a group and an offset within the group, the group is permuted by
/// the fixed order in the module documentation, and the two are recombined with the offset as the
/// high part. The lane index is untouched, which is the property that makes the layout lane
/// parallel.
///
/// # Panics
///
/// If `row` is not below `T::WIDTH` or `lane` is not below `T::LANES`.
#[inline]
#[must_use]
pub fn source_index<T: Packable>(row: usize, lane: usize) -> usize {
    assert!(row < T::WIDTH, "row {row} is outside a {} bit type", T::WIDTH);
    assert!(lane < T::LANES, "lane {lane} is outside {} lanes", T::LANES);
    let group_size = T::WIDTH / 8;
    let group = row / group_size;
    let offset = row % group_size;
    ((offset * 8) + ORDER[group]) * T::LANES + lane
}

/// Rewrites 1024 values from row order into the transposed layout.
///
/// # Errors
///
/// If either slice is not exactly [`VALUES`] long.
pub fn transpose<T: Packable>(input: &[T], output: &mut [T]) -> Result<()> {
    check_vector_len(input.len(), "input")?;
    check_vector_len(output.len(), "output")?;
    for row in 0..T::WIDTH {
        for lane in 0..T::LANES {
            output[row * T::LANES + lane] = input[source_index::<T>(row, lane)];
        }
    }
    Ok(())
}

/// Rewrites 1024 values from the transposed layout back into row order.
///
/// # Errors
///
/// If either slice is not exactly [`VALUES`] long.
pub fn untranspose<T: Packable>(input: &[T], output: &mut [T]) -> Result<()> {
    check_vector_len(input.len(), "input")?;
    check_vector_len(output.len(), "output")?;
    for row in 0..T::WIDTH {
        for lane in 0..T::LANES {
            output[source_index::<T>(row, lane)] = input[row * T::LANES + lane];
        }
    }
    Ok(())
}

/// How many words of `T` a packed vector of the given width occupies.
///
/// Every lane contributes `width` words, which is the same `width * 1024` bits the naive layout
/// would use. The layout costs nothing in space.
#[must_use]
pub fn packed_len<T: Packable>(width: usize) -> usize {
    width * T::LANES
}

/// How many bytes a whole unit of `u64` words occupies on the wire at the given width.
///
/// The serialized form of a full unit is [`packed_len`] words written little endian, so this is what
/// a reader takes out of a chunk before handing it to [`unpack_unit_into`].
#[must_use]
pub fn unit_len(width: usize) -> usize {
    packed_len::<u64>(width) * size_of::<u64>()
}

/// The smallest bit width that can hold every value in the slice. Zero for an empty slice or a
/// slice of zeros, which [`pack_transposed`] handles as the degenerate case that stores nothing.
#[must_use]
pub fn required_width<T: Packable>(values: &[T]) -> usize {
    let max = values.iter().copied().max().map_or(0, T::to_u64);
    (64 - max.leading_zeros()) as usize
}

/// Packs a transposed vector at a fixed bit width.
///
/// The input is 1024 values already in the layout [`transpose`] produces, and the output is
/// [`packed_len`] words. Every lane is packed independently and the loop over lanes is the one a
/// SIMD implementation replaces with a single register.
///
/// # Errors
///
/// If the input is not [`VALUES`] long, if the output is not [`packed_len`] long, if `width`
/// exceeds the width of the type, or if a value does not fit in `width` bits.
pub fn pack_transposed<T: Packable>(input: &[T], width: usize, output: &mut [T]) -> Result<()> {
    check_vector_len(input.len(), "input")?;
    check_width::<T>(width)?;
    if output.len() != packed_len::<T>(width) {
        return Err(Error::internal(format!(
            "a {width} bit packed vector is {} words, not {}",
            packed_len::<T>(width),
            output.len()
        )));
    }
    if width == 0 {
        // Nothing is stored. The caller has already established that every value is zero, either
        // by asking for `required_width` or by being the CONSTANT encoding, and the check below
        // enforces it rather than trusting it.
        return check_all_zero(input);
    }

    let mask = low_mask(width);
    let lanes = T::LANES;
    for lane in 0..lanes {
        // Bits already sitting in `accumulator`, always below `T::WIDTH` between iterations.
        let mut filled = 0usize;
        let mut accumulator = 0u64;
        let mut word = 0usize;
        for row in 0..T::WIDTH {
            let value = input[row * lanes + lane].to_u64();
            if value & !mask != 0 {
                return Err(Error::internal(format!("value {value} does not fit in {width} bits")));
            }
            accumulator |= value << filled;
            filled += width;
            if filled >= T::WIDTH {
                output[word * lanes + lane] = T::from_u64(accumulator & low_mask(T::WIDTH));
                word += 1;
                // The only value that can straddle the word boundary is the one just written, so
                // the carry is a shift of it rather than anything kept from earlier rows.
                let consumed = width - (filled - T::WIDTH);
                filled -= T::WIDTH;
                accumulator = shift_right(value, consumed);
            }
        }
        debug_assert_eq!(filled, 0, "a packed lane always ends on a word boundary");
    }
    Ok(())
}

/// Unpacks into the transposed layout. The inverse of [`pack_transposed`].
///
/// # Errors
///
/// If the input is not [`packed_len`] long, if the output is not [`VALUES`] long, or if `width`
/// exceeds the width of the type.
pub fn unpack_transposed<T: Packable>(input: &[T], width: usize, output: &mut [T]) -> Result<()> {
    check_width::<T>(width)?;
    check_vector_len(output.len(), "output")?;
    if input.len() != packed_len::<T>(width) {
        return Err(Error::internal(format!(
            "a {width} bit packed vector is {} words, not {}",
            packed_len::<T>(width),
            input.len()
        )));
    }
    if width == 0 {
        output.fill(T::from_u64(0));
        return Ok(());
    }

    let mask = low_mask(width);
    let lanes = T::LANES;
    for lane in 0..lanes {
        // Bits of the current word not yet handed out, right aligned in `buffer`.
        let mut available = 0usize;
        let mut buffer = 0u64;
        let mut word = 0usize;
        for row in 0..T::WIDTH {
            let value = if available >= width {
                let value = buffer & mask;
                buffer = shift_right(buffer, width);
                available -= width;
                value
            } else {
                let next = input[word * lanes + lane].to_u64();
                word += 1;
                let taken = width - available;
                let value = buffer | ((next & low_mask(taken)) << available);
                buffer = shift_right(next, taken);
                available = T::WIDTH - taken;
                value
            };
            output[row * lanes + lane] = T::from_u64(value);
        }
    }
    Ok(())
}

/// The buffer [`pack_with`] transposes through, kept so it can be reused.
///
/// Going between row order and the transposed layout needs somewhere to put the other order, and
/// that somewhere is [`VALUES`] values, which is 8 KB for a `u64`. Allocating it per call is not the
/// expensive part. Zeroing it is, because the allocator hands back a page it has to clear and the
/// transpose then writes every element of it anyway. On a scan of a packed integer column that is
/// once per 1024 rows, and it showed up as the largest single item in a ClickBench profile, larger
/// than the unpacking it was making room for.
///
/// So a caller that packs more than one unit should make one of these and pass it in. The unpacking
/// side does not need one at all any more: see [`unpack`].
///
/// It starts empty and grows on the first unit that needs it, because a caller holds one for a whole
/// decode and most chunks are not bit packed at all. Making the buffer in the constructor was tried
/// and was worse than what it replaced, by more than the zeroing it saved.
#[derive(Debug)]
pub struct Scratch<T: Packable> {
    transposed: Vec<T>,
}

impl<T: Packable> Scratch<T> {
    /// A scratch buffer that has not made room for anything yet.
    #[must_use]
    pub const fn new() -> Self {
        Self { transposed: Vec::new() }
    }

    /// Makes room for one unit. A no op every time after the first.
    fn ready(&mut self) {
        if self.transposed.len() != VALUES {
            self.transposed.resize(VALUES, T::from_u64(0));
        }
    }
}

impl<T: Packable> Default for Scratch<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Packs a vector given in row order, transposing it first.
///
/// The engine does not use this. Data written by the storage layer is transposed once on the way
/// in and stays that way, per the module documentation. This exists for tests, for the format lab,
/// and for the one place that has to hand back a vector in the order the user gave it.
///
/// # Errors
///
/// As [`pack_transposed`].
pub fn pack<T: Packable>(input: &[T], width: usize, output: &mut [T]) -> Result<()> {
    pack_with(input, width, output, &mut Scratch::new())
}

/// As [`pack`], through a buffer the caller keeps rather than one allocated per call.
///
/// # Errors
///
/// As [`pack_transposed`].
pub fn pack_with<T: Packable>(
    input: &[T],
    width: usize,
    output: &mut [T],
    scratch: &mut Scratch<T>,
) -> Result<()> {
    check_vector_len(input.len(), "input")?;
    scratch.ready();
    transpose(input, &mut scratch.transposed)?;
    pack_transposed(&scratch.transposed, width, output)
}

/// Unpacks into row order. The inverse of [`pack`].
///
/// This is what every scan of a packed integer column goes through, so it is written as one pass
/// rather than as [`unpack_transposed`] followed by [`untranspose`]. Those two are still here and
/// still the definition of the layout, and the test below checks this agrees with them at every
/// width, but running them in sequence costs three things this does not. A 1024 value buffer to
/// hold the middle, a second read of all of it, and a scatter: `untranspose` walks its input in
/// order and writes all over its output, which is a store that misses and a loop no compiler will
/// turn into wider instructions.
///
/// The fused form works because a row has the same bit schedule in every lane. That is the whole
/// point of the layout. Row `r` of every lane takes bits `r * width` to `(r + 1) * width` of that
/// lane's stream, so which word to read and how far to shift it are decided once for the row, and
/// what is left for the lanes is a load, a shift, an or, a mask and a store with no branch and no
/// carry from the lane before. The lanes of a row are next to each other in both the packed words
/// and the output, so that inner loop reads and writes straight lines. Where a row lands in the
/// output is the permutation `untranspose` was applying, and since the lane index is the low part
/// of it, it comes out as a base address for the row and costs nothing.
///
/// # Errors
///
/// As [`unpack_transposed`].
pub fn unpack<T: Packable>(input: &[T], width: usize, output: &mut [T]) -> Result<()> {
    unpack_mapped(input, width, output, T::from_u64)
}

/// As [`unpack`], putting each value through `value` on the way out.
///
/// The caller that wants this is one whose output is not the packed type. Frame of reference coding
/// stores offsets from a base and hands back the base plus the offset, and a decode that unpacks
/// into a buffer of offsets and then walks that buffer adding the base writes every value twice and
/// reads it once in between. There is nowhere for the second pass to hide: the buffer is 8 KB, a
/// chunk is about one unit, and a scan reads a chunk per part per column, so the pass is the same
/// order of work as the unpacking it follows.
///
/// The mapping belongs at the store rather than after it because that is the one place the value is
/// already in a register. Both loops below end in a store, so `value` is applied to something
/// nothing else has to load again, and for the identity it compiles to what [`unpack`] compiled to
/// before this existed.
///
/// # Errors
///
/// As [`unpack_transposed`].
pub fn unpack_mapped<T: Packable, U: Copy>(
    input: &[T],
    width: usize,
    output: &mut [U],
    value: impl Fn(u64) -> U,
) -> Result<()> {
    check_width::<T>(width)?;
    check_vector_len(output.len(), "output")?;
    if input.len() != packed_len::<T>(width) {
        return Err(Error::internal(format!(
            "a {width} bit packed vector is {} words, not {}",
            packed_len::<T>(width),
            input.len()
        )));
    }
    if width == 0 {
        output.fill(value(0));
        return Ok(());
    }

    let mask = low_mask(width);
    let lanes = T::LANES;
    let group_size = T::WIDTH / 8;
    for row in 0..T::WIDTH {
        let bit = row * width;
        let word = bit / T::WIDTH;
        let shift = bit % T::WIDTH;
        // The same arithmetic as `source_index` with the lane left off, because the lane is the low
        // part of it and the lanes of a row are consecutive from here.
        let base = ((row % group_size) * 8 + ORDER[row / group_size]) * lanes;
        let low = &input[word * lanes..(word + 1) * lanes];
        let into = &mut output[base..base + lanes];
        if shift + width <= T::WIDTH {
            for lane in 0..lanes {
                into[lane] = value((low[lane].to_u64() >> shift) & mask);
            }
        } else {
            // The value straddles two words, so `shift` is above zero, the carry in from the word
            // above is a left shift by less than the word width, and neither shift can overflow.
            // There is a word above to read: a value that straddles into word `word + 1` is one the
            // packer wrote there, and it wrote `width` words a lane.
            let carried = T::WIDTH - shift;
            let high = &input[(word + 1) * lanes..(word + 2) * lanes];
            for lane in 0..lanes {
                let bits = (low[lane].to_u64() >> shift) | (high[lane].to_u64() << carried);
                into[lane] = value(bits & mask);
            }
        }
    }
    Ok(())
}

/// As [`unpack_mapped`] for a unit that is still the bytes it was written as.
///
/// This is the form every scan of a packed integer column goes through, and the reason it exists
/// rather than the caller making a `&[u64]` first is that making one is a copy of the unit. The
/// bytes arrive inside a chunk at whatever offset the chunk put them, so they are not eight byte
/// aligned and cannot be looked at as words in place. Copying them somewhere aligned is 8 KB moved
/// per thousand rows, which is the same order of work as the unpacking it feeds and which bought
/// nothing: every word here is read exactly once, and an unaligned eight byte load is the same
/// single instruction the aligned one is on anything this runs on.
///
/// The body is [`unpack_mapped`] at `T = u64` with the loads spelled out, and the test below checks
/// the two agree at every width. It is written twice rather than made generic over where a word
/// comes from because the slice form gets its bound checked once a row and this one cannot, so a
/// shared inner loop would be the slower of the two shapes for both callers.
///
/// # Errors
///
/// If `width` exceeds 64, the output is not [`VALUES`] long, or the input is not [`unit_len`] bytes.
pub fn unpack_unit_into<U: Copy>(
    input: &[u8],
    width: usize,
    output: &mut [U],
    value: impl Fn(u64) -> U,
) -> Result<()> {
    check_width::<u64>(width)?;
    check_vector_len(output.len(), "output")?;
    if input.len() != unit_len(width) {
        return Err(Error::internal(format!(
            "a {width} bit packed vector is {} bytes, not {}",
            unit_len(width),
            input.len()
        )));
    }
    if width == 0 {
        output.fill(value(0));
        return Ok(());
    }

    let mask = low_mask(width);
    let lanes = <u64 as Packable>::LANES;
    let stride = lanes * size_of::<u64>();
    for row in 0..u64::BITS as usize {
        let bit = row * width;
        let word = bit / u64::BITS as usize;
        let shift = bit % u64::BITS as usize;
        let base = ((row % 8) * 8 + ORDER[row / 8]) * lanes;
        let low = &input[word * stride..(word + 1) * stride];
        let into = &mut output[base..base + lanes];
        if shift + width <= u64::BITS as usize {
            for (lane, slot) in into.iter_mut().enumerate() {
                *slot = value((word_at(low, lane * size_of::<u64>()) >> shift) & mask);
            }
        } else {
            let carried = u64::BITS as usize - shift;
            let high = &input[(word + 1) * stride..(word + 2) * stride];
            for (lane, slot) in into.iter_mut().enumerate() {
                let at = lane * size_of::<u64>();
                let bits = (word_at(low, at) >> shift) | (word_at(high, at) << carried);
                *slot = value(bits & mask);
            }
        }
    }
    Ok(())
}

/// Reads one value from a full transposed unit of packed `u64` values.
///
/// The serialized integer cascade stores packed words little endian. A point lookup starts from
/// the row-order index, inverts the fixed FastLanes permutation, and reads the one or two words
/// that hold that value. This is the point form of [`unpack`] for callers that need a sparse set of
/// positions rather than a materialized vector.
///
/// # Errors
///
/// If `width` exceeds 64, `index` is outside a full unit, or `input` is not the exact byte length
/// of a full unit at that width.
pub fn unpack_u64_at(input: &[u8], width: usize, index: usize) -> Result<u64> {
    let mut value = 0;
    unpack_u64_each(input, width, [index], |found| value = found)?;
    Ok(value)
}

/// [`unpack_u64_at`] for many values of one unit, which hands `each` the value at each place of
/// `indices` in turn.
///
/// The unit is checked once rather than once a value. A scan narrowed by a join reads a few rows of
/// most units of a key column, and on JOB 6f the checks and the call a value were as much of the
/// read as the loads and shifts that find the values.
///
/// # Errors
///
/// As [`unpack_u64_at`], for the first index outside a full unit.
pub fn unpack_u64_each(
    input: &[u8],
    width: usize,
    indices: impl IntoIterator<Item = usize>,
    mut each: impl FnMut(u64),
) -> Result<()> {
    check_width::<u64>(width)?;
    let expected = packed_len::<u64>(width) * size_of::<u64>();
    if input.len() != expected {
        return Err(Error::internal(format!(
            "a {width} bit packed vector is {expected} bytes, not {}",
            input.len()
        )));
    }
    for index in indices {
        if index >= VALUES {
            return Err(Error::internal(format!(
                "packed value {index} is outside a {VALUES} value unit"
            )));
        }
        each(if width == 0 { 0 } else { unit_value(input, width, index) });
    }
    Ok(())
}

/// The value at `index` of a full unit `input` at `width` bits, which the callers have checked.
#[inline]
fn unit_value(input: &[u8], width: usize, index: usize) -> u64 {
    let lanes = <u64 as Packable>::LANES;
    let block = index / lanes;
    let lane = index % lanes;
    // ORDER is its own inverse. `block` is the permuted row group and offset produced by
    // `source_index`, so applying ORDER again recovers the original group.
    let group = ORDER[block % 8];
    let row = group * (<u64 as Packable>::WIDTH / 8) + block / 8;
    let bit = row * width;
    let word = bit / <u64 as Packable>::WIDTH;
    let shift = bit % <u64 as Packable>::WIDTH;
    let low = word_at(input, (word * lanes + lane) * size_of::<u64>());
    let bits = if shift + width <= <u64 as Packable>::WIDTH {
        low >> shift
    } else {
        let high = word_at(input, ((word + 1) * lanes + lane) * size_of::<u64>());
        (low >> shift) | (high << (<u64 as Packable>::WIDTH - shift))
    };
    bits & low_mask(width)
}

/// How many bytes [`pack_tail`] writes for `count` values at `width` bits.
#[must_use]
pub fn tail_len(count: usize, width: usize) -> usize {
    (count * width).div_ceil(8)
}

/// Packs fewer than [`VALUES`] values, sequentially and to a byte boundary.
///
/// The transposed layout is all or nothing. A value lives at a row and a lane, the lanes are
/// interleaved through the whole buffer, and there is no prefix of a packed unit that holds a
/// prefix of the values. So a unit holding 3 values costs the same as a unit holding 1024, which is
/// 5 KB to store three numbers, and every nested array in a cascade is short: a dictionary of five
/// entries, a run length array, an exception list.
///
/// This is the other layout for exactly those. It is the obvious sequential one, value 0 in the low
/// bits, and it has the dependency chain the transposed layout was chosen to avoid. That is
/// affordable here and only here: a tail is at most 1023 values and is decoded once, so the chain
/// is bounded by a number that does not grow with the data, while a full unit is on the hot path of
/// every scan in the system.
///
/// # Errors
///
/// If `count` is not below [`VALUES`], if `width` exceeds 64, or if a value does not fit.
pub fn pack_tail(values: &[u64], width: usize, output: &mut Vec<u8>) -> Result<()> {
    check_tail(values.len(), width)?;
    pack_linear(values, width, output)
}

/// Packs any number of values in the layout [`pack_tail`] writes.
///
/// [`pack_tail`] is this with a bound, and the bound is a statement about columns rather than about
/// the layout: a column that has a whole unit of values has a transposed unit to put them in, so
/// the sequential layout is for the remainder and asking for it with a full unit in hand is a bug.
///
/// A key map is the other kind of caller. It is not a column, it is never decoded as a run, and
/// every read of it is a single [`tail_at`] out of the middle of a binary search, so the transposed
/// layout would buy it nothing and the bound would cost it the form: the sorted key map over
/// fifteen million `orders` rows is fifteen million values in one array addressed by index. The
/// writer's carry chain is still here and is still serial, and that is a build time cost paid once
/// over a column that is being sorted anyway.
///
/// # Errors
///
/// If `width` exceeds 64, or if a value does not fit in `width` bits.
pub fn pack_linear(values: &[u64], width: usize, output: &mut Vec<u8>) -> Result<()> {
    if width > 64 {
        return Err(Error::internal(format!("{width} bits does not fit in 64")));
    }
    if width == 0 {
        return check_all_zero(values);
    }
    let mask = low_mask(width);
    // 128 bits, because the accumulator holds up to 7 bits left over from the previous value plus a
    // whole 64 bit one.
    let mut accumulator: u128 = 0;
    let mut filled = 0usize;
    for value in values {
        if value & !mask != 0 {
            return Err(Error::internal(format!("value {value} does not fit in {width} bits")));
        }
        accumulator |= u128::from(*value) << filled;
        filled += width;
        while filled >= 8 {
            output.push((accumulator & 0xff) as u8);
            accumulator >>= 8;
            filled -= 8;
        }
    }
    if filled > 0 {
        output.push((accumulator & 0xff) as u8);
    }
    Ok(())
}

/// Unpacks what [`pack_tail`] wrote.
///
/// The writer has a dependency chain because it has to know how many bits are left over from the
/// value before, but the reader does not, and this does not carry one. Value `index` occupies the
/// `width` bits starting at bit `index * width`, so its position is arithmetic rather than history,
/// and since it begins at most seven bits into a byte and runs at most sixty four, it always lies
/// inside sixteen bytes read from that byte. One unaligned load, one shift and one mask.
///
/// That matters more than the module documentation lets on. The argument there is that a tail is at
/// most 1023 values and so is bounded by a number that does not grow with the data, which is true
/// per call and misleading in aggregate, because a cascade puts a short array in every chunk and a
/// scan reads every chunk. ClickBench 9 is where it showed. UserID is nearly unique, so its
/// dictionary holds about a thousand sixty four bit values per part and lands one value short of a
/// full unit, which sends the whole column down this path: nine hundred and seventy four parts,
/// about a million values, and the byte at a time version fed eight bytes through a `u128` for each
/// one. That was fifty five percent of the instructions of a scan of that column on its own.
///
/// # Errors
///
/// If `count` is not below [`VALUES`], if `width` exceeds 64, or if the input is shorter than
/// [`tail_len`].
pub fn unpack_tail(input: &[u8], width: usize, count: usize) -> Result<Vec<u64>> {
    // Ahead of the buffer, so that a count off a corrupt file is refused rather than allocated for.
    check_tail(count, width)?;
    let mut values = vec![0u64; count];
    unpack_tail_into(input, width, &mut values, |bits| bits)?;
    Ok(values)
}

/// As [`unpack_tail`], into a buffer the caller owns and through a mapping on the way out.
///
/// How many values to read is `output.len()`. This is the form the decoders want and
/// [`unpack_tail`] is now a wrapper over it, because a cascade calls this once per chunk and a scan
/// reads a chunk per part per column: returning a fresh `Vec` is an allocation per chunk, and
/// handing back raw offsets for the caller to add a base to in a second pass is a second write of
/// every value. Both of those are per value costs wearing the clothes of a per call one. See
/// [`unpack_mapped`] for why the mapping goes at the store.
///
/// # Errors
///
/// As [`unpack_tail`].
pub fn unpack_tail_into<U: Copy>(
    input: &[u8],
    width: usize,
    output: &mut [U],
    value: impl Fn(u64) -> U,
) -> Result<()> {
    let count = output.len();
    check_tail(count, width)?;
    if width == 0 {
        output.fill(value(0));
        return Ok(());
    }
    if input.len() < tail_len(count, width) {
        return Err(Error::internal(format!(
            "{count} values at {width} bits need {} bytes and there are {}",
            tail_len(count, width),
            input.len()
        )));
    }
    let mask = u128::from(low_mask(width));
    let read = |window: u128, bit: usize| ((window >> bit) & mask) as u64;
    // A buffer shorter than a window is one load for the whole call, because everything it holds is
    // inside it. Short arrays are most of what a cascade stores, so this is the common case by
    // count of calls even though it is the rare one by count of values.
    if input.len() < WINDOW {
        let mut window = [0u8; WINDOW];
        window[..input.len()].copy_from_slice(input);
        let word = u128::from_le_bytes(window);
        for (index, slot) in output.iter_mut().enumerate() {
            *slot = value(read(word, index * width));
        }
        return Ok(());
    }
    // Otherwise a value is read where it lies, until the window would run off the end.
    let whole = (((input.len() - WINDOW) * 8) / width + 1).min(count);
    if width <= NARROW {
        // Half the window, because a value this wide that starts at most seven bits into a byte
        // ends inside the eight bytes from that byte. The shift and the mask are then one
        // instruction each where a 128 bit shift is three, and every real width is down here: the
        // offsets a text block carries are seventeen bits and a dictionary code is fewer.
        let mask = low_mask(width);
        for (index, slot) in output[..whole].iter_mut().enumerate() {
            let bit = index * width;
            let word = word_at(input, bit / 8);
            *slot = value((word >> (bit % 8)) & mask);
        }
    } else {
        for (index, slot) in output[..whole].iter_mut().enumerate() {
            let bit = index * width;
            let mut window = [0u8; WINDOW];
            window.copy_from_slice(&input[bit / 8..bit / 8 + WINDOW]);
            *slot = value(read(u128::from_le_bytes(window), bit % 8));
        }
    }
    if whole < count {
        // Every value left over begins past the sixteenth byte from the end, by the definition of
        // `whole` just above, and the buffer stops on the byte holding the top bits of the last
        // one. So all of them lie inside the final window and one load serves the lot.
        let base = input.len() - WINDOW;
        let mut window = [0u8; WINDOW];
        window.copy_from_slice(&input[base..]);
        let word = u128::from_le_bytes(window);
        for (offset, slot) in output[whole..].iter_mut().enumerate() {
            *slot = value(read(word, (whole + offset) * width - base * 8));
        }
    }
    Ok(())
}

/// One value of a run written by [`pack_tail`], read where it lies.
///
/// [`unpack_tail`] decodes the whole run, which is what a scan wants and what nearly every caller
/// here is. A binary search is the other kind of caller: it wants one value out of the middle of a
/// block, it makes about as many probes as the block has bits, and decoding the block to answer one
/// of them would cost more than reading the value it was avoiding.
///
/// # Errors
///
/// If `width` exceeds 64, or if the value would run past the end of `input`.
#[inline(always)]
pub fn tail_at(input: &[u8], width: usize, index: usize) -> Result<u64> {
    // The common case ahead of the checks, since it needs none of them: a width of at most 56 bits
    // starting anywhere in a byte ends inside the eight bytes from that byte, and those are there.
    // This is the read a link makes for every child it is asked about. The rest is a function of
    // its own so that this much is inlined into the loops that call it. With all of it in one
    // body the compiler kept it a call, and on JOB the call a child cost as much as the read.
    let start = index.wrapping_mul(width);
    if width <= 56 && index <= usize::MAX / 64 && start / 8 + 8 <= input.len() {
        return Ok((word_at(input, start / 8) >> (start % 8)) & low_mask(width));
    }
    tail_at_rest(input, width, index)
}

/// [`tail_at`] for a value too wide for one load or too near the end of `input` for one.
#[cold]
#[inline(never)]
fn tail_at_rest(input: &[u8], width: usize, index: usize) -> Result<u64> {
    if width > 64 {
        return Err(Error::internal(format!("a width of {width} is past what a u64 holds")));
    }
    if width == 0 {
        return Ok(0);
    }
    let start = index * width;
    let end = start + width;
    if end.div_ceil(8) > input.len() {
        return Err(Error::internal(format!(
            "value {index} at {width} bits ends past the {} bytes there are",
            input.len()
        )));
    }
    let first = start / 8;
    let last = (end - 1) / 8;
    // A value that ends inside the eight bytes it starts in is one load, one shift and one mask.
    // The window below copies a length the compiler does not know, which is a call to `memcpy`
    // rather than a load, and this reads one value at a time for every string a text column hands
    // out. It was fifteen percent of ClickBench 27.
    if first + 8 <= input.len() && last - first < 8 {
        return Ok((word_at(input, first) >> (start % 8)) & low_mask(width));
    }
    let mut window = [0u8; WINDOW];
    window[..=last - first].copy_from_slice(&input[first..=last]);
    let word = u128::from_le_bytes(window);
    Ok(((word >> (start % 8)) & u128::from(low_mask(width))) as u64)
}

/// Two neighbouring values of a run, read from one load where the pair fits inside it.
///
/// `index` is the later of the two and the answer is the pair at `index - 1` and `index`. A text
/// column asks for exactly this once per string it hands out, because a value starts where the one
/// before it ended. Two calls to [`tail_at`] read the same eight bytes twice and do the bounds
/// arithmetic twice, where a pair of seventeen bit offsets, which is what a block of text carries,
/// both lie inside one load.
///
/// # Errors
///
/// If `index` is zero, if `width` exceeds 64, or if the pair would run past the end of `input`.
#[inline]
pub fn tail_pair(input: &[u8], width: usize, index: usize) -> Result<(u64, u64)> {
    let Some(before) = index.checked_sub(1) else {
        return Err(Error::internal("a tail pair has nothing before its first value"));
    };
    if width == 0 {
        return Ok((0, 0));
    }
    let start = before * width;
    let shift = start % 8;
    let first = start / 8;
    if shift + 2 * width <= u64::BITS as usize && first + 8 <= input.len() {
        let word = word_at(input, first) >> shift;
        let mask = low_mask(width);
        return Ok((word & mask, (word >> width) & mask));
    }
    Ok((tail_at(input, width, before)?, tail_at(input, width, index)?))
}

/// The bytes a single tail value can span, which is a shift of at most seven plus a width of at
/// most sixty four, so seventy one bits and therefore nine bytes, rounded up to the load that
/// covers it.
const WINDOW: usize = 16;

/// The widest value that always ends inside the eight bytes it starts in, which is sixty four bits
/// less the seven a value can begin into its first byte.
const NARROW: usize = 57;

/// Eight bytes read where they lie, as one load.
///
/// The length is a constant the compiler can see, which is what makes it a load. The caller is
/// responsible for `at + 8` being inside `input`, and the index below says so where it is not.
#[inline]
fn word_at(input: &[u8], at: usize) -> u64 {
    let run: [u8; 8] = input[at..at + 8].try_into().expect("eight bytes");
    u64::from_le_bytes(run)
}

fn check_tail(count: usize, width: usize) -> Result<()> {
    if count >= VALUES {
        return Err(Error::internal(format!(
            "{count} values is a whole unit and belongs in the transposed layout"
        )));
    }
    if width > 64 {
        return Err(Error::internal(format!("{width} bits does not fit in 64")));
    }
    Ok(())
}

fn check_vector_len(len: usize, what: &str) -> Result<()> {
    if len == VALUES {
        Ok(())
    } else {
        Err(Error::internal(format!("{what} is {len} values, and a packed unit is {VALUES}")))
    }
}

fn check_width<T: Packable>(width: usize) -> Result<()> {
    if width <= T::WIDTH {
        Ok(())
    } else {
        Err(Error::internal(format!("{width} bits does not fit in a {} bit type", T::WIDTH)))
    }
}

fn check_all_zero<T: Packable>(input: &[T]) -> Result<()> {
    match input.iter().position(|value| value.to_u64() != 0) {
        None => Ok(()),
        Some(index) => Err(Error::internal(format!(
            "a zero bit vector cannot hold {:?} at {index}",
            input[index]
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A xorshift, so that the test data is the same on every host and in every run without the
    /// workspace growing a dependency for it.
    struct Random(u64);

    impl Random {
        fn new() -> Self {
            Self(0x2545_f491_4f6c_dd1d)
        }

        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    fn sample<T: Packable>(width: usize) -> Vec<T> {
        let mut random = Random::new();
        (0..VALUES).map(|_| T::from_u64(random.next() & low_mask(width))).collect()
    }

    fn round_trip<T: Packable>(width: usize) {
        let values = sample::<T>(width);
        let mut packed = vec![T::from_u64(0); packed_len::<T>(width)];
        pack(&values, width, &mut packed).unwrap();
        let mut back = vec![T::from_u64(0); VALUES];
        unpack(&packed, width, &mut back).unwrap();
        assert_eq!(back, values, "{width} bits of a {} bit type", T::WIDTH);
    }

    #[test]
    fn every_width_of_every_type_round_trips() {
        for width in 0..=8 {
            round_trip::<u8>(width);
        }
        for width in 0..=16 {
            round_trip::<u16>(width);
        }
        for width in 0..=32 {
            round_trip::<u32>(width);
        }
        for width in 0..=64 {
            round_trip::<u64>(width);
        }
    }

    #[test]
    fn one_value_from_a_full_u64_unit_agrees_with_a_whole_unpack() {
        for width in 0..=64 {
            let values = sample::<u64>(width);
            let mut packed = vec![0u64; packed_len::<u64>(width)];
            pack(&values, width, &mut packed).unwrap();
            let bytes = packed.iter().flat_map(|word| word.to_le_bytes()).collect::<Vec<_>>();
            for (index, expected) in values.iter().enumerate() {
                assert_eq!(
                    unpack_u64_at(&bytes, width, index).unwrap(),
                    *expected,
                    "value {index} at {width} bits"
                );
            }
        }
    }

    #[test]
    fn a_unit_unpacked_from_bytes_gives_what_one_unpacked_from_words_gives() {
        // Two spellings of the same loop, one reading aligned words and one reading them where the
        // chunk left them, so every width is checked against the other rather than against a table.
        // The mapping is not the identity, because the caller this exists for is frame of reference
        // coding and a base that lands in the answer is the way a shift applied to the wrong word
        // would show up.
        for width in 0..=64 {
            let values = sample::<u64>(width);
            let mut packed = vec![0u64; packed_len::<u64>(width)];
            pack(&values, width, &mut packed).unwrap();
            let bytes = packed.iter().flat_map(|word| word.to_le_bytes()).collect::<Vec<_>>();
            assert_eq!(bytes.len(), unit_len(width), "at {width} bits");
            let map = |offset: u64| offset.wrapping_add(0x1234_5678) as i64;
            let mut from_words = vec![0i64; VALUES];
            unpack_mapped(&packed, width, &mut from_words, map).unwrap();
            let mut from_bytes = vec![0i64; VALUES];
            unpack_unit_into(&bytes, width, &mut from_bytes, map).unwrap();
            assert_eq!(from_bytes, from_words, "at {width} bits");
            // And the same again with the bytes handed over at an odd offset, which is where a chunk
            // puts them and which is the whole reason this form reads them a word at a time.
            let mut moved = vec![0u8; bytes.len() + 3];
            moved[3..].copy_from_slice(&bytes);
            let mut from_moved = vec![0i64; VALUES];
            unpack_unit_into(&moved[3..], width, &mut from_moved, map).unwrap();
            assert_eq!(from_moved, from_words, "at {width} bits, three bytes along");
        }
    }

    #[test]
    fn a_unit_of_the_wrong_length_is_refused() {
        let mut out = vec![0i64; VALUES];
        let bytes = vec![0u8; unit_len(9) - 1];
        assert!(unpack_unit_into(&bytes, 9, &mut out, |bits| bits as i64).is_err());
        let bytes = vec![0u8; unit_len(9) + 1];
        assert!(unpack_unit_into(&bytes, 9, &mut out, |bits| bits as i64).is_err());
        let bytes = vec![0u8; unit_len(65)];
        assert!(unpack_unit_into(&bytes, 65, &mut out, |bits| bits as i64).is_err());
        let bytes = vec![0u8; unit_len(9)];
        assert!(unpack_unit_into(&bytes, 9, &mut out[..VALUES - 1], |bits| bits as i64).is_err());
    }

    #[test]
    fn a_reused_scratch_gives_what_a_fresh_one_gives() {
        // The buffer a unit transposes through is handed in so it is not zeroed per call, which is
        // only sound if every element of it is written every time. If some were not, a narrow unit
        // following a wide one would read whatever the wide one left behind, so the widths here go
        // up and down rather than in order and each answer is checked against the same unit packed
        // through a buffer nothing has touched.
        let mut scratch = Scratch::<u64>::new();
        for width in [64, 1, 33, 7, 64, 0, 17, 60, 3] {
            let values = sample::<u64>(width);
            let mut reused = vec![0u64; packed_len::<u64>(width)];
            pack_with(&values, width, &mut reused, &mut scratch).unwrap();
            let mut fresh = vec![0u64; packed_len::<u64>(width)];
            pack(&values, width, &mut fresh).unwrap();
            assert_eq!(reused, fresh, "at {width} bits after a wider unit");
            let mut back = vec![0u64; VALUES];
            unpack(&reused, width, &mut back).unwrap();
            assert_eq!(back, values, "at {width} bits");
        }
    }

    #[test]
    fn the_one_pass_unpack_gives_what_the_two_passes_give() {
        // `unpack` is the fused form of `unpack_transposed` followed by `untranspose`, and those two
        // are the definition of the layout. So this checks the fast one against the slow one at
        // every width of every type rather than against a remembered answer, which is the check that
        // would catch the fused one getting a shift or a row base wrong at one width out of sixty
        // five.
        fn agree<T: Packable>() {
            for width in 0..=T::WIDTH {
                let values = sample::<T>(width);
                let mut transposed = vec![T::from_u64(0); VALUES];
                transpose(&values, &mut transposed).unwrap();
                let mut packed = vec![T::from_u64(0); packed_len::<T>(width)];
                pack_transposed(&transposed, width, &mut packed).unwrap();

                let mut middle = vec![T::from_u64(0); VALUES];
                unpack_transposed(&packed, width, &mut middle).unwrap();
                let mut slow = vec![T::from_u64(0); VALUES];
                untranspose(&middle, &mut slow).unwrap();

                let mut fast = vec![T::from_u64(0); VALUES];
                unpack(&packed, width, &mut fast).unwrap();

                assert_eq!(fast, slow, "{} bit type at {width} bits", T::WIDTH);
                assert_eq!(fast, values, "{} bit type at {width} bits round trip", T::WIDTH);
            }
        }
        agree::<u8>();
        agree::<u16>();
        agree::<u32>();
        agree::<u64>();
    }

    #[test]
    fn a_mapped_unpack_gives_what_unpacking_and_then_mapping_gives() {
        // The frame of reference decode is the caller, so the mapping under test is the one it
        // uses: a signed base added to an unsigned offset, into an output of a different type from
        // the packed words. Checked at every width because the two inner loops of `unpack_mapped`
        // split on whether a value straddles two words, and which one runs depends on the width.
        for width in 0..=64 {
            let values = sample::<u64>(width);
            let mut packed = vec![0u64; packed_len::<u64>(width)];
            pack(&values, width, &mut packed).unwrap();

            let base = -7i64;
            let mut mapped = vec![0i64; VALUES];
            unpack_mapped(&packed, width, &mut mapped, |offset| {
                (i128::from(base) + i128::from(offset)) as i64
            })
            .unwrap();

            let mut plain = vec![0u64; VALUES];
            unpack(&packed, width, &mut plain).unwrap();
            let expected: Vec<i64> = plain
                .iter()
                .map(|offset| (i128::from(base) + i128::from(*offset)) as i64)
                .collect();
            assert_eq!(mapped, expected, "at {width} bits");
        }
    }

    #[test]
    fn a_mapped_tail_gives_what_unpacking_the_tail_and_then_mapping_gives() {
        // Every length, because `unpack_tail_into` has three paths through it and which one a call
        // takes depends on how many bytes the run came to: everything inside one window, a walk
        // that stops a window short of the end, and the leftovers after that walk.
        for width in [0usize, 1, 7, 17, 32, 57, 58, 64] {
            for count in [1usize, 2, 63, 64, 300, 1023] {
                let values: Vec<u64> = sample::<u64>(width).into_iter().take(count).collect();
                let mut packed = Vec::new();
                pack_tail(&values, width, &mut packed).unwrap();

                let base = 11i64;
                let mut mapped = vec![0i64; count];
                unpack_tail_into(&packed, width, &mut mapped, |offset| {
                    (i128::from(base) + i128::from(offset)) as i64
                })
                .unwrap();

                let plain = unpack_tail(&packed, width, count).unwrap();
                let expected: Vec<i64> = plain
                    .iter()
                    .map(|offset| (i128::from(base) + i128::from(*offset)) as i64)
                    .collect();
                assert_eq!(mapped, expected, "{count} values at {width} bits");
            }
        }
    }

    #[test]
    fn the_transposed_form_also_round_trips_without_being_reordered() {
        // What the engine actually does: transpose once, then pack and unpack any number of times
        // without ever going back to row order.
        let values = sample::<u32>(19);
        let mut transposed = vec![0u32; VALUES];
        transpose(&values, &mut transposed).unwrap();
        let mut packed = vec![0u32; packed_len::<u32>(19)];
        pack_transposed(&transposed, 19, &mut packed).unwrap();
        let mut back = vec![0u32; VALUES];
        unpack_transposed(&packed, 19, &mut back).unwrap();
        assert_eq!(back, transposed);
    }

    #[test]
    fn the_permutation_is_a_bijection() {
        // Every value has to land somewhere and no two may land in the same place, or a round trip
        // would silently drop rows. Checked for all four widths because the group size changes.
        fn check<T: Packable>() {
            let mut seen = vec![false; VALUES];
            for row in 0..T::WIDTH {
                for lane in 0..T::LANES {
                    let index = source_index::<T>(row, lane);
                    assert!(!seen[index], "{index} is written twice for {} bits", T::WIDTH);
                    seen[index] = true;
                }
            }
            assert!(seen.into_iter().all(|hit| hit));
        }
        check::<u8>();
        check::<u16>();
        check::<u32>();
        check::<u64>();
    }

    #[test]
    fn transposing_is_not_the_identity() {
        // If it were, the test above would be passing on a layout that is not the FastLanes one.
        let values: Vec<u32> = (0..VALUES).map(|index| index as u32).collect();
        let mut transposed = vec![0u32; VALUES];
        transpose(&values, &mut transposed).unwrap();
        assert_ne!(transposed, values);
        let mut back = vec![0u32; VALUES];
        untranspose(&transposed, &mut back).unwrap();
        assert_eq!(back, values);
    }

    #[test]
    fn a_full_width_pack_is_the_data_itself() {
        // 64 bits of a 64 bit type has no packing to do, and the loop that handles the general case
        // has to get the degenerate one right rather than shifting by 64 and wrapping.
        let values = sample::<u64>(64);
        let mut transposed = vec![0u64; VALUES];
        transpose(&values, &mut transposed).unwrap();
        let mut packed = vec![0u64; packed_len::<u64>(64)];
        pack_transposed(&transposed, 64, &mut packed).unwrap();
        assert_eq!(packed, transposed);
    }

    #[test]
    fn a_zero_width_vector_stores_nothing_and_reads_back_as_zeros() {
        let values = vec![0u32; VALUES];
        assert_eq!(required_width(&values), 0);
        let mut packed = Vec::new();
        pack(&values, 0, &mut packed).unwrap();
        let mut back = vec![7u32; VALUES];
        unpack(&packed, 0, &mut back).unwrap();
        assert_eq!(back, values);
    }

    #[test]
    fn required_width_is_the_bits_of_the_largest_value() {
        assert_eq!(required_width::<u32>(&[]), 0);
        assert_eq!(required_width::<u32>(&[0, 0]), 0);
        assert_eq!(required_width::<u32>(&[1]), 1);
        assert_eq!(required_width::<u32>(&[255, 3]), 8);
        assert_eq!(required_width::<u32>(&[256]), 9);
        assert_eq!(required_width::<u64>(&[u64::MAX]), 64);
    }

    #[test]
    fn a_value_too_wide_for_the_width_is_an_error_rather_than_silent_truncation() {
        let mut values = vec![0u32; VALUES];
        values[500] = 8;
        let mut transposed = vec![0u32; VALUES];
        transpose(&values, &mut transposed).unwrap();
        let mut packed = vec![0u32; packed_len::<u32>(3)];
        let error = pack_transposed(&transposed, 3, &mut packed).unwrap_err();
        assert!(error.message().contains("does not fit in 3 bits"), "{error}");
    }

    #[test]
    fn a_wrong_sized_buffer_is_an_error() {
        let values = vec![0u32; VALUES];
        let mut packed = vec![0u32; 3];
        let error = pack(&values, 5, &mut packed).unwrap_err();
        assert!(error.message().contains("words"), "{error}");

        let short = vec![0u32; 7];
        let mut output = vec![0u32; VALUES];
        let error = unpack(&short, 5, &mut output).unwrap_err();
        assert!(error.message().contains("words"), "{error}");
    }

    #[test]
    fn a_nonzero_value_at_zero_width_is_an_error() {
        let mut values = vec![0u32; VALUES];
        values[9] = 1;
        let mut packed = Vec::new();
        let error = pack(&values, 0, &mut packed).unwrap_err();
        assert!(error.message().contains("zero bit vector"), "{error}");
    }

    #[test]
    fn packing_at_a_width_the_type_cannot_hold_is_an_error() {
        let values = vec![0u16; VALUES];
        let mut packed = vec![0u16; 17 * 64];
        let error = pack(&values, 17, &mut packed).unwrap_err();
        assert!(error.message().contains("16 bit type"), "{error}");
    }

    #[test]
    fn a_tail_round_trips_at_every_width_and_every_length() {
        let mut random = Random::new();
        for width in 0..=64usize {
            for count in [0usize, 1, 2, 7, 8, 9, 100, 1023] {
                let values: Vec<u64> =
                    (0..count).map(|_| random.next() & low_mask(width)).collect();
                let mut bytes = Vec::new();
                pack_tail(&values, width, &mut bytes).unwrap();
                assert_eq!(bytes.len(), tail_len(count, width), "{count} at {width}");
                assert_eq!(
                    unpack_tail(&bytes, width, count).unwrap(),
                    values,
                    "{count} at {width}"
                );
            }
        }
    }

    /// Reading one value where it lies agrees with decoding the whole run.
    ///
    /// Every width and every position, since the point of it is the arithmetic that finds the bytes
    /// a value straddles, and that is what is off by one.
    ///
    /// A value that runs off the buffer is an error. The buffer stops on a byte boundary and a value
    /// does not, so an index a little past the count can still lie inside the padding of the last
    /// byte and that reads rather than complains. It is the caller that knows how many values it
    /// wrote, the same way it does for `unpack_tail`.
    #[test]
    fn one_value_of_a_tail_reads_the_same_as_the_whole_of_it() {
        let mut random = Random::new();
        for width in 0..=64usize {
            let count = 37;
            let values: Vec<u64> = (0..count).map(|_| random.next() & low_mask(width)).collect();
            let mut bytes = Vec::new();
            pack_tail(&values, width, &mut bytes).unwrap();
            for (index, value) in values.iter().enumerate() {
                assert_eq!(tail_at(&bytes, width, index).unwrap(), *value, "{index} at {width}");
            }
            let Some(fits) = (bytes.len() * 8).checked_div(width) else { continue };
            assert!(tail_at(&bytes, width, fits + 1).is_err(), "past the end at {width}");
        }
    }

    /// The two halves of the reader agree with each other.
    ///
    /// A value is read with one sixteen byte load, which the values near the end of the buffer
    /// cannot have because the buffer stops on the byte holding the top bits of the last one. Those
    /// go through a zero padded copy instead, and the split between the two is arithmetic on
    /// lengths, which is the kind of thing that is off by one. Handing the same bytes to the reader
    /// twice, once exactly sized so the last values take the padded path and once with slack on the
    /// end so every value takes the fast one, makes the two paths check each other at every width.
    #[test]
    fn the_padded_end_of_a_tail_reads_the_same_as_the_windowed_start() {
        let mut random = Random::new();
        for width in 1..=64usize {
            for count in [1usize, 2, 3, 17, 129, 1023] {
                let values: Vec<u64> =
                    (0..count).map(|_| random.next() & low_mask(width)).collect();
                let mut exact = Vec::new();
                pack_tail(&values, width, &mut exact).unwrap();
                let mut slack = exact.clone();
                slack.extend_from_slice(&[0u8; WINDOW]);
                assert_eq!(
                    unpack_tail(&exact, width, count).unwrap(),
                    values,
                    "{count} at {width}"
                );
                assert_eq!(
                    unpack_tail(&slack, width, count).unwrap(),
                    values,
                    "{count} at {width}"
                );
            }
        }
    }

    /// The pair read agrees with two single reads, at every width and every position.
    ///
    /// The pair has its own arithmetic for the case where both values fit one load, so the thing to
    /// check is that it falls back to the same answer everywhere that does not hold, which is every
    /// width past thirty two and every value near the end of the buffer.
    #[test]
    fn a_pair_of_tail_values_reads_the_same_as_the_two_of_them_apart() {
        let mut random = Random::new();
        for width in 0..=64usize {
            let count = 37;
            let values: Vec<u64> = (0..count).map(|_| random.next() & low_mask(width)).collect();
            let mut bytes = Vec::new();
            pack_tail(&values, width, &mut bytes).unwrap();
            for index in 1..count {
                assert_eq!(
                    tail_pair(&bytes, width, index).unwrap(),
                    (values[index - 1], values[index]),
                    "{index} at {width}"
                );
            }
            assert!(tail_pair(&bytes, width, 0).is_err(), "nothing before the first at {width}");
        }
    }

    #[test]
    fn a_tail_costs_its_own_values_and_not_a_whole_unit() {
        // The reason it exists. Three 40 bit values in the transposed layout is a 5 KB buffer.
        let values = vec![(1u64 << 39) + 1; 3];
        let mut bytes = Vec::new();
        pack_tail(&values, 40, &mut bytes).unwrap();
        assert_eq!(bytes.len(), 15);
        assert_eq!(packed_len::<u64>(40) * 8, 5120);
    }

    #[test]
    fn a_whole_unit_is_refused_by_the_tail_packer() {
        let values = vec![0u64; VALUES];
        let error = pack_tail(&values, 4, &mut Vec::new()).unwrap_err();
        assert!(error.message().contains("whole unit"), "{error}");
    }

    #[test]
    fn a_short_tail_buffer_is_an_error() {
        let error = unpack_tail(&[0, 0], 8, 5).unwrap_err();
        assert!(error.message().contains("need 5 bytes"), "{error}");
    }

    #[test]
    fn the_packed_size_is_the_same_as_the_naive_layout() {
        for width in 0..=32 {
            assert_eq!(packed_len::<u32>(width) * 32, width * VALUES);
        }
    }
}
