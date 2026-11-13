// Lifted from rudb crates/rudb-graph/src/link.rs at cd9f9676 (2026-10-05).
// Changes: errors of malformed bytes are XX001 `Error::corrupted`.

//! The forward link: for each child row, the [`Rid`] of its parent.
//!
//! spec/graph/03-the-file-format.md section 3.4. One per relationship, in child `rid` order, with
//! [`NO_PARENT`] for a child whose key matched nothing. Two physical forms, chosen by measuring the
//! column rather than by declaring it, exactly as the key map's three are.
//!
//! [`Form::Packed`] is the general one: the parent `rid`s bit-packed to `ceil(log2(parent + 1))`
//! bits, with the maximum representable value meaning no parent. Random access is a shift and a
//! mask. It answers the child to parent direction and nothing else.
//!
//! [`Form::Monotone`] is the one that makes the budget in section 3.7 livable. When the child table
//! is physically clustered by the join key, which is how `dbgen` emits `lineitem` against `orders`
//! and `partsupp` against `part`, the parent `rid`s are non-decreasing and storing one integer per
//! child is a waste. The payload becomes a bit vector of `children + parents` bits: for each parent
//! in `rid` order, a run of one bits, one per child that points at it, then a zero. On TPC-H SF100
//! that is 93.8 MB against 2.10 GB for the packed form of the same relationship, and it answers
//! both directions, so the backward adjacency of section 3.5 does not have to exist for it.
//!
//! # The two formulas, and the convention that decides them
//!
//! Section 3.4 gives `forward(child) = rank0(select1(child))` and a backward formula beside it. The
//! two are only both true under one layout, and the one that makes the forward formula right is
//! ones first: parent zero's children, then a zero, then parent one's children, then a zero. Under
//! that layout a child's one bit has exactly as many zeros before it as its parent has `rid`, which
//! is the forward formula, and the backward formula is `[cum(parent - 1), cum(parent))` where
//! `cum(p) = select0(p) - p` is the number of children of every parent up to and including `p`.
//! The forward direction is the hot one, so it is the one the layout is chosen for.
//!
//! # What the monotone form refuses
//!
//! A child with no parent. Under the packed form that is a reserved value; under this one there is
//! nowhere to put it, because every bit is either a child of the parent whose run it is in or a
//! parent boundary. A relationship with an unmatched child is therefore packed even when its
//! matched children are in order, which is the honest answer and is what
//! [`Link::build`] does without being asked.
//!
//! # Where the part-skip statistic comes from
//!
//! Section 5.5 prunes a whole part during a semi-join reduction using the minimum and maximum
//! parent `rid` in that part. Section 3.4 expected that for free from the zone map of a stored
//! column. This link is a section payload rather than a stored column, for the reason section 3.8
//! sanctions: the parent's key map has to exist before the child's link can be built, so the link
//! is built in a second pass at checkpoint time, and a second pass can append a section to a
//! committed file but cannot go back and add a column to its stripes. So the statistic is stored
//! here instead, as a minimum and a maximum per [`PART_ROWS`] children, which is sixteen bytes per
//! thousand and change rows. The monotone form stores none, because a non-decreasing sequence's
//! minimum and maximum over a range are its two ends and two selects are cheaper than nine
//! megabytes.

use rupg_common::{Error, Result};
use rupg_encode::bitpack;

use crate::bits::BitVector;
use crate::rid::{NO_PARENT, PART_ROWS, Rid};
use crate::tail::Tail;

/// The payload layout version. See the same constant in `wire.rs` for why it is belt and braces.
const LAYOUT: u8 = 1;

/// Bytes of fixed header at the front of a forward link payload.
///
/// `children`, `parents`, `linked`, then the four bytes that say what shape the rest is.
pub const HEADER_BYTES: usize = 32;

/// The three counts a forward link's header holds and the form of the body behind them. See
/// [`Link::counts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    /// Rows in the child table.
    pub children: u64,
    /// Rows in the parent table.
    pub parents: u64,
    /// Children that found a parent.
    pub linked: u64,
    /// Which form the body behind it takes.
    pub form: Form,
}

/// Which physical form a forward link took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    /// One bit-packed parent `rid` per child, maximum value reserved for no parent.
    Packed,
    /// A bit vector of runs, one run per parent, answering both directions.
    Monotone,
}

impl Form {
    /// The byte that names this form in a section's `flags`.
    #[must_use]
    pub fn tag(self) -> u8 {
        match self {
            Self::Packed => 0,
            Self::Monotone => 1,
        }
    }

    /// What `rudb_links()` calls this form.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Packed => "packed",
            Self::Monotone => "monotone",
        }
    }

    /// The form a tag names.
    ///
    /// # Errors
    ///
    /// If the tag is not one this build knows, which by section 3.2 means a section to ignore.
    pub fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            0 => Ok(Self::Packed),
            1 => Ok(Self::Monotone),
            _ => Err(malformed(format!("forward link form {tag} is not one this build knows"))),
        }
    }
}

/// The minimum and maximum parent `rid` over one part of the child table.
///
/// `None` for a part in which no child has a parent, which is a part a reduction can skip outright
/// rather than a part whose bounds happen to be empty.
pub type Bounds = Option<(Rid, Rid)>;

#[derive(Debug, Clone)]
enum Body {
    Packed {
        /// Bit-packed parent `rid`s, `width` bits each.
        bytes: Tail,
        width: usize,
        /// Minimum and maximum per [`PART_ROWS`] children, for section 5.5.
        heads: Vec<Bounds>,
    },
    Monotone {
        vector: BitVector,
    },
}

/// A relationship's child to parent map.
#[derive(Debug, Clone)]
pub struct Link {
    children: u64,
    parents: u64,
    linked: u64,
    body: Body,
}

/// Where [`Link::backward_from`] last stopped.
#[derive(Debug, Clone, Copy, Default)]
pub struct Cursor {
    at: Option<At>,
}

#[derive(Debug, Clone, Copy)]
struct At {
    parent: Rid,
    /// The bit of the zero that ends this parent's run.
    zero: usize,
    from: Rid,
    to: Rid,
}

impl Cursor {
    /// How many parents on a cursor reads rather than selects. A run is a few bits for a parent of
    /// a few children, so this is a few words at most.
    const NEAR: Rid = 64;
}

/// The bit of the `k`th zero, counting from zero, at or after bit `start`. `None` past the end of
/// the words, which a caller inside the vector's zero count never reaches.
fn kth_zero(words: &[u64], start: usize, k: u64) -> Option<usize> {
    let mut left = k;
    let mut word = start / 64;
    let mut zeros = !*words.get(word)? & (u64::MAX << (start % 64));
    loop {
        let here = u64::from(zeros.count_ones());
        if left < here {
            // Under sixty four, since it is under the count of one word.
            return Some(word * 64 + crate::bits::nth_set(zeros, left as u32) as usize);
        }
        left -= here;
        word += 1;
        zeros = !*words.get(word)?;
    }
}

impl Link {
    /// Builds a link from one parent `rid` per child, with [`NO_PARENT`] for the unmatched.
    ///
    /// The form is chosen here and not by the caller: monotone when every child has a parent and
    /// the `rid`s are non-decreasing, packed otherwise. That decision is a pass over the slice the
    /// caller already produced, so it costs a comparison per child on top of a build that was
    /// already linear.
    ///
    /// # Errors
    ///
    /// If a parent `rid` is not [`NO_PARENT`] and is not below `parents`, which means the link and
    /// the key map it was built against disagree about how many rows the parent table has. That is
    /// a bug rather than a data condition, and a link built past the end of its parent resolves to
    /// a row that is not there, which is the one failure in this layer that is a wrong answer.
    pub fn build(parents_of: &[Rid], parents: u64) -> Result<Self> {
        let children = count(parents_of.len());
        let mut linked = 0_u64;
        let mut monotone = true;
        let mut previous = 0_u64;
        for parent in parents_of {
            if *parent == NO_PARENT {
                monotone = false;
                continue;
            }
            if *parent >= parents {
                return Err(malformed(format!(
                    "a forward link points at parent {parent} of a table with {parents} rows"
                )));
            }
            if *parent < previous {
                monotone = false;
            }
            previous = *parent;
            linked += 1;
        }
        let body = if monotone && parents > 0 {
            Body::Monotone { vector: runs(parents_of, parents)? }
        } else {
            packed(parents_of, parents)?
        };
        Ok(Self { children, parents, linked, body })
    }

    /// Which form the build chose.
    #[must_use]
    pub fn form(&self) -> Form {
        match self.body {
            Body::Packed { .. } => Form::Packed,
            Body::Monotone { .. } => Form::Monotone,
        }
    }

    /// Rows in the child table.
    #[must_use]
    pub fn children(&self) -> u64 {
        self.children
    }

    /// Rows in the parent table.
    #[must_use]
    pub fn parents(&self) -> u64 {
        self.parents
    }

    /// Children that found a parent.
    #[must_use]
    pub fn linked(&self) -> u64 {
        self.linked
    }

    /// The bits of the monotone form, one run of ones per parent in `rid` order and a zero after
    /// each, or `None` for the packed form.
    ///
    /// For a push that wants whole parents at a time rather than one child at a time, see
    /// [`crate::rids::Rids::forward`].
    pub(crate) fn runs(&self) -> Option<&BitVector> {
        match &self.body {
            Body::Monotone { vector } => Some(vector),
            Body::Packed { .. } => None,
        }
    }

    /// Bytes the body costs, not counting the header.
    #[must_use]
    pub fn bytes(&self) -> usize {
        match &self.body {
            Body::Packed { bytes, heads, .. } => bytes.len() + heads.len() * 16,
            Body::Monotone { vector } => vector.bytes(),
        }
    }

    /// The parent of a child row, or `None` if it has none or the child is past the end.
    #[must_use]
    pub fn forward(&self, child: Rid) -> Option<Rid> {
        if child >= self.children {
            return None;
        }
        match &self.body {
            Body::Packed { bytes, width, .. } => {
                // `tail_at` fails on a width past sixty four or an index past the end, and the
                // bound above plus the width check at read rule out both, so an error here is not a
                // condition to report but a bug. Section 3.1 says the answer to a broken section is
                // no section, and `None` is what no section says about a child.
                let value = bitpack::tail_at(bytes, *width, usize::try_from(child).ok()?).ok()?;
                (value != reserved(*width)).then_some(value)
            }
            Body::Monotone { vector } => {
                let at = vector.select1(child)?;
                Some(vector.rank0(at))
            }
        }
    }

    /// The parents of a run of consecutive children, with [`NO_PARENT`] for the ones that have none.
    ///
    /// This is [`Link::forward`] over a range, and what it adds is the monotone form. Answering one
    /// child there is a `select1`, which is a search, and walking a run of them one search at a time
    /// is paying for random access on a read that is sequential. So the run is found once and then
    /// read off the bitmap in order: a one bit is a child of the current parent and a zero bit moves
    /// on to the next parent, which is a load per sixty four bits and a count of zeros per word.
    ///
    /// # Errors
    ///
    /// If the run goes past the last child.
    pub fn forward_run(&self, first: Rid, out: &mut [Rid]) -> Result<()> {
        let end = first.checked_add(count(out.len()));
        if end.is_none_or(|end| end > self.children) {
            return Err(Error::internal(format!(
                "a run of {} children from {first} goes past the {} the link has",
                out.len(),
                self.children
            )));
        }
        if out.is_empty() {
            return Ok(());
        }
        match &self.body {
            Body::Packed { bytes, width, .. } => {
                let absent = reserved(*width);
                let start = usize::try_from(first)
                    .map_err(|_| malformed("a child past what fits in memory"))?;
                for (at, slot) in out.iter_mut().enumerate() {
                    let value = bitpack::tail_at(bytes, *width, start + at)?;
                    *slot = if value == absent { NO_PARENT } else { value };
                }
            }
            Body::Monotone { vector } => {
                // Every child has a parent in this form, so the first child's parent is the count
                // of zeros before its bit and every later one follows from the bits in between.
                let Some(mut at) = vector.select1(first) else {
                    return Err(malformed("a monotone link has fewer ones than children"));
                };
                let mut parent = vector.rank0(at);
                let words = vector.words();
                for slot in out.iter_mut() {
                    // Skip the zeros up to the next one bit, a word at a time, counting each as a
                    // parent boundary crossed.
                    loop {
                        let word = words.get(at / 64).copied().unwrap_or(0) >> (at % 64);
                        if word == 0 {
                            let skipped = 64 - at % 64;
                            parent += count(skipped);
                            at += skipped;
                            if at >= vector.len() {
                                return Err(malformed("a monotone link ran out of ones"));
                            }
                            continue;
                        }
                        let zeros = word.trailing_zeros() as usize;
                        parent += count(zeros);
                        at += zeros;
                        break;
                    }
                    *slot = parent;
                    at += 1;
                }
            }
        }
        Ok(())
    }

    /// The parent of each child in a list, with [`NO_PARENT`] for a child that has none or is past
    /// the end.
    ///
    /// This is [`Link::forward`] over the rows a filter or a reduction left, and it is for the
    /// monotone form again. A child there is a `select1` and a `rank0`, about three hundred
    /// instructions between them, and on q09 those were a tenth of the query for 319,404 children.
    /// The children a scan hands up are ascending, so the next one is usually a few words further
    /// along the bitmap than the last, and walking those words is a count of ones per word. A
    /// child further away than `Link::WALK` children, or one before the last, is searched for
    /// again, so the answer is the same in any order and only the cost depends on it.
    pub fn forward_each(&self, children: &[Rid], out: &mut Vec<Rid>) {
        out.clear();
        out.reserve(children.len());
        let vector = match &self.body {
            Body::Monotone { vector } => vector,
            // The body matched once for the chunk rather than once a child, with the same answers
            // as `forward`.
            Body::Packed { bytes, width, .. } => {
                let absent = reserved(*width);
                out.extend(children.iter().map(|&child| {
                    if child >= self.children {
                        return NO_PARENT;
                    }
                    usize::try_from(child)
                        .ok()
                        .and_then(|child| bitpack::tail_at(bytes, *width, child).ok())
                        .filter(|&value| value != absent)
                        .unwrap_or(NO_PARENT)
                }));
                return;
            }
        };
        let words = vector.words();
        // The last child answered, the position of its bit and its parent.
        let mut last: Option<(Rid, usize, Rid)> = None;
        for &child in children {
            if child >= self.children {
                out.push(NO_PARENT);
                continue;
            }
            let near = last.filter(|&(from, ..)| child >= from && child - from <= Self::WALK);
            let (at, parent) = match near {
                Some((from, at, parent)) => {
                    walk_ones(words, at, parent, child - from).unwrap_or((usize::MAX, NO_PARENT))
                }
                None => match vector.select1(child) {
                    Some(at) => (at, vector.rank0(at)),
                    None => (usize::MAX, NO_PARENT),
                },
            };
            if parent == NO_PARENT {
                last = None;
            } else {
                last = Some((child, at, parent));
            }
            out.push(parent);
        }
    }

    /// How many children [`Link::forward_each`] walks the bitmap across before it searches instead.
    ///
    /// A word walked is about five instructions and holds a few dozen children on a table with a
    /// few children per parent, and a search is about three hundred, so the break even is some
    /// thousands of children and this stays well under it.
    const WALK: Rid = 1024;

    /// The children of a parent row, as a half open range of child `rid`s.
    ///
    /// `None` for the packed form, which does not answer this direction, and for a parent past the
    /// end. An empty range is a parent with no children and is not the same answer.
    #[must_use]
    pub fn backward(&self, parent: Rid) -> Option<std::ops::Range<Rid>> {
        let Body::Monotone { vector } = &self.body else { return None };
        if parent >= self.parents {
            return None;
        }
        // `cum(p)` is the children of every parent up to and including `p`: the `p`th zero has `p`
        // zeros and every earlier one bit before it, so subtracting the zeros leaves the ones.
        let cum = |nth: Rid| -> Option<u64> { vector.select0(nth).map(|at| count(at) - nth) };
        let from = if parent == 0 { 0 } else { cum(parent - 1)? };
        Some(from..cum(parent)?)
    }

    /// [`Self::backward`] for a parent at or a little past the one `cursor` was left at, found by
    /// reading on from there rather than by two selects.
    ///
    /// For a caller asking about parents in rising order, which is a child read in its own order
    /// asking about its siblings. A select is a search each time, and on TPC-H q21 two of them per
    /// line of `lineitem` were a tenth of the query, where the next parent asked about is a word or
    /// two of bits further on. A parent behind the cursor or far past it is two selects as before,
    /// and leaves the cursor there.
    #[must_use]
    pub fn backward_from(&self, parent: Rid, cursor: &mut Cursor) -> Option<std::ops::Range<Rid>> {
        let Body::Monotone { vector } = &self.body else { return None };
        if parent >= self.parents {
            return None;
        }
        if let Some(at) = cursor.at
            && at.parent == parent
        {
            return Some(at.from..at.to);
        }
        let near = cursor.at.filter(|at| at.parent < parent && parent - at.parent <= Cursor::NEAR);
        let found = match near {
            Some(at) => {
                let words = vector.words();
                // Every run ends in a zero, so the parents in between are that many zeros on, and
                // the ones passed on the way are their children.
                let start = at.zero + 1;
                let between = parent - at.parent - 1;
                let (first, from) = if between == 0 {
                    (start, at.to)
                } else {
                    let before = kth_zero(words, start, between - 1)?;
                    (before + 1, at.to + count(before - start) - (between - 1))
                };
                let zero = kth_zero(words, first, 0)?;
                At { parent, zero, from, to: from + count(zero - first) }
            }
            None => {
                let zero = vector.select0(parent)?;
                let to = count(zero) - parent;
                let from =
                    if parent == 0 { 0 } else { count(vector.select0(parent - 1)?) - (parent - 1) };
                At { parent, zero, from, to }
            }
        };
        cursor.at = Some(found);
        Some(found.from..found.to)
    }

    /// The minimum and maximum parent `rid` over one part of the child table, for section 5.5.
    ///
    /// `None` for a part past the end of the table; `Some(None)` for a part in which no child has a
    /// parent, which is a part a reduction skips.
    #[must_use]
    pub fn part_bounds(&self, part: usize) -> Option<Bounds> {
        let first = count(part * PART_ROWS);
        if first >= self.children {
            return None;
        }
        match &self.body {
            Body::Packed { heads, .. } => heads.get(part).copied(),
            // A non-decreasing sequence's extremes over a range are its ends, so this is two
            // selects rather than the nine megabytes SF100 would spend storing them.
            Body::Monotone { .. } => {
                let last = (first + count(PART_ROWS) - 1).min(self.children - 1);
                match (self.forward(first), self.forward(last)) {
                    (Some(low), Some(high)) => Some(Some((low, high))),
                    _ => Some(None),
                }
            }
        }
    }

    /// Appends the header and the body.
    ///
    /// # Errors
    ///
    /// If a length does not fit the width the layout gives it.
    pub fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        let start = out.len();
        out.extend_from_slice(&self.children.to_le_bytes());
        out.extend_from_slice(&self.parents.to_le_bytes());
        out.extend_from_slice(&self.linked.to_le_bytes());
        out.push(self.form().tag());
        out.push(match &self.body {
            // The width is derivable from `parents`, and it is written anyway, because a packed
            // array read at the wrong width is not an error but a page of plausible wrong numbers.
            // This is the one place in this layer where a number stored twice earns its keep.
            Body::Packed { width, .. } => u8::try_from(*width)
                .map_err(|_| malformed("a forward link wider than a byte can name"))?,
            Body::Monotone { .. } => 0,
        });
        out.push(LAYOUT);
        // Five bytes of nothing, so that the header is thirty two and the body behind it starts on
        // an eight byte boundary. The monotone form's body is an array of `u64`s and the packed
        // form's head is pairs of them, and a payload whose reader has to handle both an aligned
        // and an unaligned case for no reason is a payload with a second code path in it.
        out.extend_from_slice(&[0; 5]);
        debug_assert_eq!(
            out.len() - start,
            HEADER_BYTES,
            "the forward link header is thirty two bytes"
        );
        match &self.body {
            Body::Packed { bytes, heads, .. } => {
                for head in heads {
                    let (low, high) = head.unwrap_or((NO_PARENT, NO_PARENT));
                    out.extend_from_slice(&low.to_le_bytes());
                    out.extend_from_slice(&high.to_le_bytes());
                }
                out.extend_from_slice(bytes);
            }
            Body::Monotone { vector } => vector.write(out),
        }
        Ok(())
    }

    /// The counts at the front of a link's payload, read without its body.
    ///
    /// `bytes` is at least the first [`HEADER_BYTES`] of what [`Link::write`] produced, and may be
    /// all of it. This is for a caller that only wants to know whether every child found a parent,
    /// which a planner asks of every relationship before a query, and which is three numbers at the
    /// front of a body that is megabytes long for the links of a large table.
    ///
    /// # Errors
    ///
    /// If there are fewer bytes than a header, or the header names a form or a layout this build
    /// does not know, which is what [`Link::read`] refuses the header for too.
    pub fn counts(bytes: &[u8]) -> Result<Counts> {
        if bytes.len() < HEADER_BYTES {
            return Err(malformed("a forward link payload is shorter than its header"));
        }
        let form = Form::from_tag(bytes[24])?;
        if bytes[26] != LAYOUT {
            return Err(malformed(format!(
                "forward link layout {} is not one this build knows",
                bytes[26]
            )));
        }
        Ok(Counts {
            children: number(&bytes[0..8])?,
            parents: number(&bytes[8..16])?,
            linked: number(&bytes[16..24])?,
            form,
        })
    }

    /// Reads a link from exactly the bytes [`Link::write`] produced.
    ///
    /// # Errors
    ///
    /// If the payload is shorter than its header, names a form or a layout this build does not
    /// know, or holds a body that is not the size its header implies. Every one of those is a
    /// section to drop rather than a query to fail, by section 3.1.
    pub fn read(bytes: &[u8]) -> Result<Self> {
        Self::read_from(bytes.to_vec(), 0)
    }

    /// [`Link::read`] of the bytes of `payload` from `at` on, keeping `payload` for the packed
    /// parents rather than copying them out of it. See `Tail` for why.
    ///
    /// # Errors
    ///
    /// As [`Link::read`], or if `at` is past the end of `payload`.
    pub fn read_from(payload: Vec<u8>, at: usize) -> Result<Self> {
        let bytes = payload.get(at..).ok_or_else(|| malformed("a forward link header is torn"))?;
        let Counts { children, parents, linked, form } = Self::counts(bytes)?;
        let width = bytes[25] as usize;
        let rest = &bytes[HEADER_BYTES..];
        let body = match form {
            Form::Packed => {
                if width != width_for(parents) {
                    return Err(malformed(
                        "a forward link's width is not the one its parents imply",
                    ));
                }
                let parts = usize::try_from(children.div_ceil(count(PART_ROWS)))
                    .map_err(|_| malformed("a forward link with more parts than fit in memory"))?;
                let head = parts * 16;
                let rows = usize::try_from(children)
                    .map_err(|_| malformed("a forward link longer than fits in memory"))?;
                let packed = bitpack::tail_len(rows, width);
                if rest.len() != head + packed {
                    return Err(malformed(
                        "a forward link's body is not the size its header implies",
                    ));
                }
                let mut heads = Vec::with_capacity(parts);
                for part in 0..parts {
                    let low = number(&rest[part * 16..part * 16 + 8])?;
                    let high = number(&rest[part * 16 + 8..part * 16 + 16])?;
                    heads.push((low != NO_PARENT).then_some((low, high)));
                }
                let bytes = Tail::of(payload, at + HEADER_BYTES + head).ok_or_else(|| {
                    malformed("a forward link's body is not the size its header implies")
                })?;
                Body::Packed { bytes, width, heads }
            }
            Form::Monotone => {
                let len = usize::try_from(linked + parents)
                    .map_err(|_| malformed("a forward link longer than fits in memory"))?;
                Body::Monotone { vector: BitVector::read(rest, len)? }
            }
        };
        Ok(Self { children, parents, linked, body })
    }
}

/// The bit vector of runs, one run of ones per parent, each terminated by a zero.
fn runs(parents_of: &[Rid], parents: u64) -> Result<BitVector> {
    let len = usize::try_from(count(parents_of.len()) + parents)
        .map_err(|_| malformed("a forward link longer than fits in memory"))?;
    let mut words = vec![0_u64; len.div_ceil(64)];
    let mut at = 0_usize;
    let mut child = 0_usize;
    for parent in 0..parents {
        while child < parents_of.len() && parents_of[child] == parent {
            words[at / 64] |= 1 << (at % 64);
            at += 1;
            child += 1;
        }
        at += 1;
    }
    debug_assert_eq!(at, len, "every child is a one and every parent is a zero");
    BitVector::new(words, len)
}

/// The bit-packed array, plus the per-part bounds section 5.5 asks for.
///
/// The substituted values are collected before packing rather than written in place, because
/// `bitpack::pack_linear` takes a slice and the packer's carry chain is what makes it worth
/// reusing. That is eight bytes per child held twice for the length of one call, on top of the
/// slice the caller already built, and it is the reason the monotone form matters: this is the
/// path SF100's `lineitem` against `part` takes and it is expensive in both directions.
fn packed(parents_of: &[Rid], parents: u64) -> Result<Body> {
    let width = width_for(parents);
    let absent = reserved(width);
    let mut heads = Vec::with_capacity(parents_of.len().div_ceil(PART_ROWS));
    for rows in parents_of.chunks(PART_ROWS) {
        let mut bounds: Bounds = None;
        for parent in rows {
            if *parent == NO_PARENT {
                continue;
            }
            bounds = Some(match bounds {
                None => (*parent, *parent),
                Some((low, high)) => (low.min(*parent), high.max(*parent)),
            });
        }
        heads.push(bounds);
    }
    let values = parents_of
        .iter()
        .map(|parent| if *parent == NO_PARENT { absent } else { *parent })
        .collect::<Vec<u64>>();
    let mut bytes = Vec::with_capacity(bitpack::tail_len(values.len(), width));
    bitpack::pack_linear(&values, width, &mut bytes)?;
    Ok(Body::Packed { bytes: bytes.into(), width, heads })
}

/// Bits per entry: enough for every parent `rid` and one more value meaning no parent.
///
/// The bit length of `parents`, which is `ceil(log2(parents + 1))` written the way a CPU computes
/// it. A table of three parents has `rid`s zero, one and two and needs a fourth value for absent,
/// which is two bits exactly; a table of four needs three.
fn width_for(parents: u64) -> usize {
    (u64::BITS - parents.leading_zeros()).max(1) as usize
}

/// The value that means no parent, which is the largest the width can hold.
fn reserved(width: usize) -> u64 {
    if width >= 64 { u64::MAX } else { (1_u64 << width) - 1 }
}

/// A row count as a `u64`, which is what every count in a header is.
/// The position and parent of the one bit `skip` ones after the one at `at`, whose parent is
/// `parent`, or `None` if the bitmap runs out first.
///
/// Each zero crossed is a parent boundary. A word at a time: shift out the bits at or before the
/// current one, and either the ones left in the word are too few, so count them and its zeros and
/// move on, or the one wanted is in it.
fn walk_ones(words: &[u64], at: usize, mut parent: Rid, skip: Rid) -> Option<(usize, Rid)> {
    if skip == 0 {
        return Some((at, parent));
    }
    let mut left = skip - 1;
    let mut from = at + 1;
    loop {
        let index = from / 64;
        let offset = from % 64;
        let word = *words.get(index)? >> offset;
        let span = 64 - offset;
        let ones = u64::from(word.count_ones());
        if ones > left {
            #[expect(clippy::cast_possible_truncation, reason = "under the ones in one word")]
            let within = crate::bits::nth_set(word, left as u32) as usize;
            // The zeros between `from` and the one found are the parents crossed.
            parent += count(within) - left;
            return Some((from + within, parent));
        }
        // The caller checked the child against the count of children, so the one wanted is in
        // the words and the tail past the length is never reached.
        parent += count(span) - ones;
        left -= ones;
        from += span;
    }
}

fn count(rows: usize) -> u64 {
    u64::try_from(rows).unwrap_or(u64::MAX)
}

fn number(bytes: &[u8]) -> Result<u64> {
    Ok(u64::from_le_bytes(
        bytes.try_into().map_err(|_| malformed("a forward link header is torn"))?,
    ))
}

fn malformed(message: impl Into<String>) -> Error {
    Error::corrupted(format!("invalid forward link: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_finds_the_same_children_as_a_select() {
        // Parents of zero to nine children, some childless, over several words of runs.
        let mut parents_of = Vec::new();
        for parent in 0..3_000_u64 {
            for _ in 0..(parent * 7 + parent / 13) % 10 {
                parents_of.push(parent);
            }
        }
        let link = Link::build(&parents_of, 3_000).expect("build");
        assert_eq!(link.form(), Form::Monotone);
        let mut asked: Vec<Rid> = (0..3_000).step_by(3).collect();
        // Repeats, steps of one, a step back, a jump past what the cursor reads on over, the last.
        asked.extend([2_000, 2_000, 2_001, 2_002, 5, 6, 2_900, 2_999, 0, 1_000, 1_064, 1_129]);
        let mut cursor = Cursor::default();
        for parent in asked {
            assert_eq!(link.backward_from(parent, &mut cursor), link.backward(parent), "{parent}");
        }
        assert_eq!(link.backward_from(3_000, &mut cursor), None);
    }

    /// Checks every child resolves to the parent it was built from, in the link and in a copy of it
    /// that went through the payload.
    fn resolves(parents_of: &[Rid], parents: u64) -> Link {
        let built = Link::build(parents_of, parents).expect("build");
        let mut bytes = Vec::new();
        built.write(&mut bytes).expect("write");
        let read = Link::read(&bytes).expect("read");
        let counts = Link::counts(&bytes[..HEADER_BYTES]).expect("the header alone");
        assert_eq!(
            counts,
            Counts {
                children: built.children(),
                parents: built.parents(),
                linked: built.linked(),
                form: built.form()
            }
        );
        assert_eq!(read.form(), built.form(), "the form survives the round trip");
        assert_eq!(read.children(), built.children());
        assert_eq!(read.parents(), built.parents());
        assert_eq!(read.linked(), built.linked());
        for link in [&built, &read] {
            for (child, parent) in parents_of.iter().enumerate() {
                let want = (*parent != NO_PARENT).then_some(*parent);
                assert_eq!(link.forward(child as Rid), want, "child {child}");
            }
            assert_eq!(link.forward(parents_of.len() as Rid), None, "past the last child");
        }
        built
    }

    #[test]
    fn a_clustered_child_takes_the_monotone_form_and_answers_both_directions() {
        // Three children of parent zero, none of parent one, two of parent two. This is the shape
        // `lineitem` has against `orders` and it is the whole reason the form exists.
        let link = resolves(&[0, 0, 0, 2, 2], 3);
        assert_eq!(link.form(), Form::Monotone);
        assert_eq!(link.backward(0), Some(0..3));
        assert_eq!(link.backward(1), Some(3..3), "a parent with no children, not a missing parent");
        assert_eq!(link.backward(2), Some(3..5));
        assert_eq!(link.backward(3), None, "past the last parent");
    }

    #[test]
    fn an_unclustered_child_takes_the_packed_form_and_answers_one_direction() {
        let link = resolves(&[4, 1, 4, 0, 2], 5);
        assert_eq!(link.form(), Form::Packed);
        assert_eq!(link.backward(0), None, "the packed form does not answer backward");
    }

    #[test]
    fn a_child_with_no_parent_keeps_the_link_out_of_the_monotone_form() {
        // Every bit of the monotone vector is a child or a parent boundary, so there is nowhere to
        // put an unmatched child. Ordered children plus one orphan is packed, and that is honest
        // rather than a missed opportunity.
        let link = resolves(&[0, 1, NO_PARENT, 2], 3);
        assert_eq!(link.form(), Form::Packed);
        assert_eq!(link.linked(), 3, "the orphan is not linked and the other three are");
    }

    #[test]
    fn every_child_pointing_at_one_parent_is_one_run() {
        let link = resolves(&[7; 50], 8);
        assert_eq!(link.form(), Form::Monotone);
        assert_eq!(link.backward(6), Some(0..0));
        assert_eq!(link.backward(7), Some(0..50));
    }

    #[test]
    fn a_link_with_no_children_builds_and_resolves_nothing() {
        let link = resolves(&[], 10);
        assert_eq!(link.children(), 0);
        assert_eq!(link.forward(0), None);
        assert_eq!(link.part_bounds(0), None, "there is no part zero of an empty table");
    }

    #[test]
    fn a_link_whose_parent_table_is_empty_is_packed_and_matches_nothing() {
        // Not monotone: a bit vector of zero runs has nowhere to put a child. The packed form with
        // one reserved value is the answer, and every child is unmatched, which is what a link
        // against an empty parent means.
        let link = resolves(&[NO_PARENT, NO_PARENT], 0);
        assert_eq!(link.form(), Form::Packed);
        assert_eq!(link.linked(), 0);
    }

    #[test]
    fn a_parent_rid_past_the_parent_table_is_refused_rather_than_stored() {
        // The one failure in this layer that is a wrong answer rather than a slow one: a link built
        // past the end of its parent resolves to a row that is not there.
        let error = Link::build(&[0, 9], 5).expect_err("refused");
        assert!(error.to_string().contains("parent 9"), "{error}");
    }

    #[test]
    fn the_reserved_value_is_not_a_parent_rid_even_at_the_width_boundary() {
        // Three parents need two bits for rids zero, one and two, and a third value for no parent,
        // which is four values and therefore two bits exactly. Four parents need three.
        assert_eq!(width_for(3), 2);
        assert_eq!(width_for(4), 3);
        assert_eq!(reserved(2), 3);
        let link = resolves(&[2, 0, NO_PARENT], 3);
        assert_eq!(link.form(), Form::Packed);
    }

    #[test]
    fn a_packed_link_carries_the_bounds_of_every_part() {
        let mut parents_of = vec![0_u64; PART_ROWS * 2 + 5];
        for (child, parent) in parents_of.iter_mut().enumerate() {
            // Descending within each part, so the part is not monotone and its bounds are not its
            // ends, which is what the stored head is for.
            *parent = (PART_ROWS - child % PART_ROWS) as u64;
        }
        let link = Link::build(&parents_of, PART_ROWS as u64 + 1).expect("build");
        assert_eq!(link.form(), Form::Packed);
        assert_eq!(link.part_bounds(0), Some(Some((1, PART_ROWS as u64))));
        assert_eq!(link.part_bounds(2), Some(Some((PART_ROWS as u64 - 4, PART_ROWS as u64))));
        assert_eq!(link.part_bounds(3), None, "there is no fourth part");
    }

    #[test]
    fn a_part_in_which_nothing_matched_is_reported_as_skippable() {
        let mut parents_of = vec![NO_PARENT; PART_ROWS * 2];
        parents_of[PART_ROWS] = 3;
        let link = Link::build(&parents_of, 10).expect("build");
        assert_eq!(link.part_bounds(0), Some(None), "a part a reduction can skip outright");
        assert_eq!(link.part_bounds(1), Some(Some((3, 3))));
    }

    #[test]
    fn a_monotone_link_derives_its_part_bounds_from_its_ends() {
        let parents_of = (0..PART_ROWS as u64 * 2).map(|child| child / 4).collect::<Vec<Rid>>();
        let link = Link::build(&parents_of, PART_ROWS as u64).expect("build");
        assert_eq!(link.form(), Form::Monotone);
        assert_eq!(link.part_bounds(0), Some(Some((0, (PART_ROWS as u64 - 1) / 4))));
        assert_eq!(
            link.part_bounds(1),
            Some(Some((PART_ROWS as u64 / 4, (PART_ROWS as u64 * 2 - 1) / 4)))
        );
    }

    #[test]
    fn the_monotone_form_is_a_bit_per_child_and_the_packed_form_is_a_rid_per_child() {
        // Section 3.4's arithmetic in miniature. Ten thousand children of a thousand parents: the
        // packed form is ten bits each and the monotone form is one bit each plus one per parent.
        let ordered = (0..10_000_u64).map(|child| child / 10).collect::<Vec<Rid>>();
        let monotone = Link::build(&ordered, 1000).expect("build");
        assert_eq!(monotone.form(), Form::Monotone);
        let mut shuffled = ordered.clone();
        shuffled.swap(0, 9999);
        let packed = Link::build(&shuffled, 1000).expect("build");
        assert_eq!(packed.form(), Form::Packed);
        assert!(
            monotone.bytes() * 4 < packed.bytes(),
            "monotone {} is not far below packed {}",
            monotone.bytes(),
            packed.bytes()
        );
    }

    #[test]
    fn a_payload_shorter_than_its_header_is_refused() {
        let link = Link::build(&[0, 1], 2).expect("build");
        let mut bytes = Vec::new();
        link.write(&mut bytes).expect("write");
        for cut in [0, 1, HEADER_BYTES - 1] {
            assert!(Link::read(&bytes[..cut]).is_err(), "a payload of {cut} bytes is refused");
            assert!(Link::counts(&bytes[..cut]).is_err(), "a header of {cut} bytes is refused");
        }
    }

    #[test]
    fn a_form_or_a_layout_this_build_does_not_know_is_refused() {
        let link = Link::build(&[0, 1], 2).expect("build");
        let mut bytes = Vec::new();
        link.write(&mut bytes).expect("write");
        let mut wrong = bytes.clone();
        wrong[24] = 9;
        assert!(Link::read(&wrong).is_err(), "an unknown form is refused");
        assert!(Link::counts(&wrong).is_err(), "and its counts are not read");
        let mut wrong = bytes;
        wrong[26] = LAYOUT + 1;
        assert!(Link::read(&wrong).is_err(), "an unknown layout is refused");
    }

    #[test]
    fn a_width_that_does_not_match_the_parents_is_refused_rather_than_read_at() {
        // A packed array read at the wrong width is a page of plausible wrong numbers rather than
        // an error, which is why the width is both stored and checked against what it should be.
        let link = Link::build(&[1, 0], 2).expect("build");
        let mut bytes = Vec::new();
        link.write(&mut bytes).expect("write");
        bytes[25] = 7;
        let error = Link::read(&bytes).expect_err("refused");
        assert!(error.to_string().contains("width"), "{error}");
    }

    #[test]
    fn a_truncated_body_is_refused_for_either_form() {
        for parents_of in [vec![0_u64, 0, 1, 2], vec![2_u64, 0, 1, 0]] {
            let link = Link::build(&parents_of, 3).expect("build");
            let mut bytes = Vec::new();
            link.write(&mut bytes).expect("write");
            let short = &bytes[..bytes.len() - 1];
            assert!(Link::read(short).is_err(), "a truncated {:?} body is refused", link.form());
        }
    }

    /// A run decodes to what the per child lookup says, from every starting point and across long
    /// stretches of parents with no children, which is where the word at a time walk could slip.
    #[test]
    fn a_run_agrees_with_the_per_child_lookup_in_both_forms() {
        let mut clustered = Vec::new();
        for parent in 0..400_u64 {
            // Parents with no children in runs of up to a few hundred, so a walk crosses whole
            // words of zeros.
            let children = if parent % 50 < 45 { 0 } else { parent % 7 + 1 };
            clustered.extend(std::iter::repeat_n(parent, children as usize));
        }
        let scattered: Vec<Rid> = (0..3000_u64)
            .map(|child| if child % 13 == 0 { NO_PARENT } else { (child * 37) % 500 })
            .collect();
        for (parents_of, parents) in [(clustered, 400), (scattered, 500)] {
            let link = Link::build(&parents_of, parents).expect("build");
            let children = link.children();
            for first in [0, 1, 63, 64, 65, children / 2, children - 1] {
                for len in [0, 1, 2, 100, children - first] {
                    let len = len.min(children - first);
                    let mut out = vec![0; len as usize];
                    link.forward_run(first, &mut out).expect("in range");
                    let want: Vec<Rid> = (first..first + len)
                        .map(|child| link.forward(child).unwrap_or(NO_PARENT))
                        .collect();
                    assert_eq!(out, want, "{:?} from {first} for {len}", link.form());
                }
            }
            let mut out = vec![0; 2];
            assert!(link.forward_run(children - 1, &mut out).is_err(), "past the last child");
        }
    }

    /// A list of children decodes to what the per child lookup says, whether the list goes up in
    /// small steps, in steps past the walk, backwards, or past the last child.
    #[test]
    fn a_list_agrees_with_the_per_child_lookup_in_both_forms() {
        let mut clustered = Vec::new();
        for parent in 0..3000_u64 {
            let children = if parent % 50 < 45 { parent % 3 } else { parent % 7 + 1 };
            clustered.extend(std::iter::repeat_n(parent, children as usize));
        }
        let scattered: Vec<Rid> = (0..3000_u64)
            .map(|child| if child % 13 == 0 { NO_PARENT } else { (child * 37) % 500 })
            .collect();
        for (parents_of, parents) in [(clustered, 3000), (scattered, 500)] {
            let link = Link::build(&parents_of, parents).expect("build");
            let children = link.children();
            let mut lists: Vec<Vec<Rid>> = vec![
                Vec::new(),
                (0..children).collect(),
                (0..children).step_by(7).collect(),
                (0..children).step_by(1500).collect(),
                (0..children).rev().step_by(11).collect(),
                vec![5, 5, 4, children - 1, children, children + 9, 0, 63, 64, 65],
            ];
            let mut state = 7_u64;
            let mut sparse = Vec::new();
            for child in 0..children {
                state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                if state >> 60 == 0 {
                    sparse.push(child);
                }
            }
            lists.push(sparse);
            let mut out = Vec::new();
            for list in &lists {
                link.forward_each(list, &mut out);
                let want: Vec<Rid> =
                    list.iter().map(|&child| link.forward(child).unwrap_or(NO_PARENT)).collect();
                assert_eq!(out, want, "{:?} over {} children", link.form(), list.len());
            }
        }
    }
}
