// Lifted from rudb crates/rudb-graph/src/keymap.rs at cd9f9676 (2026-10-05).
// Changes: errors of malformed bytes are XX001 `Error::corrupted`.

//! Turning a parent key value into a [`Rid`].
//!
//! A link is built from an equality between a child column and a parent column, and to build it the
//! parent column's values have to become row ids. That map is the key map. It has the three
//! physical forms of spec/graph/02-the-data-model.md section 2.2, chosen by measurement at build
//! time rather than by declaration, and the form that was chosen is recorded in the header so that
//! a reader does not have to guess.
//!
//! The three exist because they are three different answers to the same question and the cheapest
//! one is usually available:
//!
//! - [`Form::Identity`] when the keys are exactly `base .. base + n` in order. Nothing is stored
//!   but two numbers, and TPC-H hits this on six of its eight tables.
//! - [`Form::Dense`] when the keys are distinct integers packed densely enough into a range that a
//!   bitmap plus a rank index beats storing them.
//! - [`Form::Permuted`] when the keys are dense in their range the way the second form needs, but
//!   the rows are not stored in key order. The same bitmap and rank, and then a permutation from
//!   rank to `rid`, because a rank is a `rid` only while the rows are in key order.
//! - [`Form::Sorted`] for everything else, including every string key, which arrives here as
//!   dictionary codes rather than as text.
//!
//! The fourth exists because the stored order is a choice, and spec/graph/12-the-order-the-suite-
//! asks-for.md section 12.5 makes it one. `orders` stored by date holds the same keys `orders`
//! stored by key does, and a map that answered one in constant time and needed the budget four times
//! over for the other would make the layer depend on the order it was supposed to be free to pick.
//!
//! What is deliberately absent is a hash. A minimal perfect hash is faster to probe than the sorted
//! form and much slower to build, and there is no measurement yet saying the probe is where the
//! time goes. spec/graph/11-open-questions.md keeps it open, and adding it later costs nothing
//! because the form is a tag in a header that a reader is already required to be able to not
//! recognize.

use rupg_common::{Error, Result};
use rupg_encode::bitpack;

use crate::bits::{Rank, nth_set};
use crate::rid::{NO_PARENT, Rid};

/// How dense a range has to be before the bitmap form beats the sorted form.
///
/// One in eight, per section 2.2. Below it the bitmap is larger than storing the keys: a bitmap
/// costs `range / 8` bytes plus about an eighth again for the rank index, and the sorted form costs
/// `count` keys plus `count` permutation entries, so the crossover is a ratio rather than a size.
/// The default is here as a named constant rather than inline because it is a number somebody will
/// want to move once there is a measurement that says where, and moving it should be a diff.
pub const DENSE_THRESHOLD: u64 = 8;

/// Which of the three physical forms a key map took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    /// `rid = key - base`, and nothing is stored but `base` and the count.
    Identity,
    /// `rid = rank(key - base)` over a bitmap of the range, with a two level rank index.
    Dense,
    /// Binary search over the sorted keys, then a permutation lookup.
    Sorted,
    /// `rid = perm[rank(key - base)]`, the dense form with a permutation behind it.
    Permuted,
}

impl Form {
    /// The tag this form takes in a section header.
    #[must_use]
    pub fn tag(self) -> u8 {
        match self {
            Self::Identity => 0,
            Self::Dense => 1,
            Self::Sorted => 2,
            Self::Permuted => 3,
        }
    }

    /// What this form is called where a person reads it, which is `rudb_links()`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Dense => "dense",
            Self::Sorted => "sorted",
            Self::Permuted => "permuted",
        }
    }

    /// The form a header tag names.
    ///
    /// # Errors
    ///
    /// If the tag is not one of the three. A reader that meets an unfamiliar form has met a file
    /// written by a later build, and the right response is the one section 3.2 requires of an
    /// unfamiliar section kind: ignore this key map and answer the query without it. So this
    /// returns an error and the caller drops the section rather than failing the open.
    pub fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            0 => Ok(Self::Identity),
            1 => Ok(Self::Dense),
            2 => Ok(Self::Sorted),
            3 => Ok(Self::Permuted),
            _ => Err(malformed(format!("key map form {tag} is not one this build knows"))),
        }
    }
}

/// What the build saw while it read the parent key column.
///
/// This is the cardinality verification of section 2.3, and it is written into the header rather
/// than recomputed because the build already had every value in front of it. Recording what was
/// observed rather than what was declared is what keeps a wrong `FOREIGN KEY` from producing a
/// wrong answer: a declaration that fails verification is reported, and no link is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observed {
    /// Non-null values seen.
    pub rows: u64,
    /// Nulls seen, which are not keys and match no child row.
    pub nulls: u64,
    /// Whether every non-null value was distinct. False means no link may be built at all.
    pub distinct: bool,
    /// Whether the values arrived in non-decreasing order.
    pub sorted: bool,
    /// The smallest non-null value, or `None` when there were none.
    pub min: Option<i128>,
    /// The largest non-null value, or `None` when there were none.
    pub max: Option<i128>,
}

impl Observed {
    /// Whether this column can be the parent side of a link.
    ///
    /// Distinctness is the whole requirement. A parent side that is not unique is not an error and
    /// is not a link: section 2.3 says it is a relationship that has to be executed as an ordinary
    /// join, and the planner is told so rather than left to find out.
    #[must_use]
    pub fn usable_as_parent(&self) -> bool {
        self.distinct
    }
}

/// The three forms, behind one interface.
#[derive(Debug, Clone)]
enum Body {
    Identity {
        base: i128,
        count: u64,
    },
    Dense {
        base: i128,
        range: u64,
        bits: Vec<u64>,
        rank: Rank,
    },
    Sorted {
        /// The smallest key, so that every stored key is a `u64` offset from it whatever the
        /// column's own type was.
        base: i128,
        /// Bits one stored key offset takes.
        key_width: usize,
        /// The key offsets in ascending order, bit packed.
        keys: Vec<u8>,
        /// Bits one permutation entry takes, which is `ceil(log2(rows))`.
        rid_width: usize,
        /// Sorted position to `rid`, bit packed.
        perm: Vec<u8>,
        count: u64,
    },
    Permuted {
        base: i128,
        range: u64,
        bits: Vec<u64>,
        rank: Rank,
        /// Bits one permutation entry takes, which is `ceil(log2(rows))`.
        rid_width: usize,
        /// Rank to `rid`, bit packed, one entry per key.
        perm: Vec<u8>,
    },
}

/// A map from a parent key value to the `rid` of the row that holds it.
#[derive(Debug, Clone)]
pub struct KeyMap {
    body: Body,
    observed: Observed,
}

impl KeyMap {
    /// Builds the cheapest correct form for these keys.
    ///
    /// `keys` is the parent key column in `rid` order, with `None` for a null. The `rid` of a value
    /// is its index, which is what makes this the whole build: the caller has already read the
    /// column in append order, so the row ids are the positions and there is nothing to look up.
    ///
    /// String keys arrive here as dictionary codes rather than as text, per section 2.2. That is
    /// not a convenience, it is the reason a sorted key map over a `VARCHAR` column never touches a
    /// byte of text: the codes of a file wide stable dictionary are integers with the column's own
    /// order, so the search is over `u32`.
    ///
    /// # Errors
    ///
    /// If the column's values span more than a `u64`, if it holds more rows than a `u64` of
    /// `rid`s, or if a bit packed payload cannot be written. A non-distinct column is not an
    /// error: it produces a key map whose [`Observed`] says so, and the caller is expected to ask
    /// before building a link on it.
    pub fn build(keys: &[Option<i128>]) -> Result<Self> {
        let mut observed = observe(keys);
        // See [`KeyMap::build_from`], which takes the same shortcut for the same reason and is
        // where the reason is written. The two paths agree on every column or a table's key map
        // depends on which of them built it.
        if !observed.distinct {
            return Ok(Self { body: Body::Identity { base: 0, count: 0 }, observed });
        }
        Ok(match plan(&observed)? {
            Plan::Empty => Self { body: Body::Identity { base: 0, count: 0 }, observed },
            Plan::Identity { base, count } => {
                Self { body: Body::Identity { base, count }, observed }
            }
            Plan::Dense { base, range } => Self { body: dense(keys, base, range)?, observed },
            Plan::Permuted { base, range } => {
                let mut bits = DenseBits::new(base, range);
                for key in keys.iter().flatten() {
                    if !bits.mark(*key)? {
                        observed.distinct = false;
                        return Ok(Self { body: Body::Identity { base: 0, count: 0 }, observed });
                    }
                }
                let mut perm = Permutation::new(bits, keys.len())?;
                for (rid, key) in keys.iter().enumerate() {
                    if let Some(key) = *key {
                        perm.place(key, rid)?;
                    }
                }
                Self { body: perm.finish()?, observed }
            }
            Plan::Sorted { base } => {
                // The sorted form sorts, so it is the one place distinctness can be settled for a
                // column that did not arrive in order. `observe` can only see an adjacent
                // duplicate; this sees every duplicate, and the answer replaces the guess.
                let (body, distinct) = sorted(keys, base, observed.rows)?;
                observed.distinct = distinct;
                Self { body, observed }
            }
        })
    }

    /// Builds the cheapest correct form by reading the column rather than by holding it.
    ///
    /// The same build as [`KeyMap::build`] and the same decision, taken from a source that can be
    /// scanned twice instead of from a slice that is already in memory. That difference is the
    /// whole reason this exists. A parent key column at TPC-H SF10 is fifteen million rows of
    /// `orders`, and a `Vec<Option<i128>>` of those is four hundred and eighty megabytes held for
    /// the length of a build that does not need a single one of them twice. At SF100 it is four and
    /// a half gigabytes, which is not a slow build, it is a build that does not happen.
    ///
    /// So the first scan observes and nothing else, and what the second scan does depends on what
    /// the first one found. The identity form, which is the form every TPC-H parent key takes,
    /// needs no second scan at all: the four observed facts are the whole map. The dense form fills
    /// a bitmap sized from the range, which is bounded by the table rather than by the scan. Only
    /// the sorted form has to hold the column, because sorting is what it is, and it says so here
    /// rather than surprising a caller with it.
    ///
    /// # Errors
    ///
    /// If the scan fails, or for any of the reasons [`KeyMap::build`] fails.
    pub fn build_from<K: Keys + ?Sized>(keys: &K) -> Result<Self> {
        let mut observer = Observer::new();
        keys.scan(&mut |key| {
            observer.push(key);
            Ok(())
        })?;
        let mut observed = observer.observed;
        // A column the first scan already saw a repeat in gets no body at all. No form answers a
        // rid for a key that is in two rows, so every byte spent on one is spent on a map nothing
        // may use, and the bytes are not small: TPC-H SF10 `lineitem(l_orderkey)` sorts sixty
        // million keys into three hundred and ninety megabytes before the budget throws all of it
        // away. This is only reachable where the duplicates are adjacent, which is where the column
        // arrived in order, and that is the case this is for. A repeat that only the sort can find
        // is still found by the sort, below.
        if !observed.distinct {
            return Ok(Self { body: Body::Identity { base: 0, count: 0 }, observed });
        }
        Ok(match plan(&observed)? {
            Plan::Empty => Self { body: Body::Identity { base: 0, count: 0 }, observed },
            Plan::Identity { base, count } => {
                Self { body: Body::Identity { base, count }, observed }
            }
            Plan::Dense { base, range } => {
                let mut bits = DenseBits::new(base, range);
                keys.scan(&mut |key| match key {
                    Some(key) => bits.push(key),
                    None => Ok(()),
                })?;
                Self { body: bits.finish(), observed }
            }
            Plan::Permuted { base, range } => {
                // Three scans rather than one that holds the column. The second marks the bitmap,
                // and is where a repeat the first scan could not see is found, since a key is
                // marked twice only when two rows hold it. The third places each row at its key's
                // rank. Holding the column instead would be sixteen bytes a row for the length of
                // the build, which is the cost `build_from` exists to avoid.
                let mut bits = DenseBits::new(base, range);
                let mut repeated = false;
                keys.scan(&mut |key| {
                    if let Some(key) = key {
                        repeated |= !bits.mark(key)?;
                    }
                    Ok(())
                })?;
                if repeated {
                    observed.distinct = false;
                    return Ok(Self { body: Body::Identity { base: 0, count: 0 }, observed });
                }
                let rows = observed.rows + observed.nulls;
                let rows = usize::try_from(rows)
                    .map_err(|_| malformed("the column is too long for this machine"))?;
                let mut perm = Permutation::new(bits, rows)?;
                let mut rid = 0_usize;
                keys.scan(&mut |key| {
                    if let Some(key) = key {
                        perm.place(key, rid)?;
                    }
                    rid += 1;
                    Ok(())
                })?;
                Self { body: perm.finish()?, observed }
            }
            Plan::Sorted { base } => {
                let mut held = Vec::with_capacity(
                    usize::try_from(observed.rows + observed.nulls).unwrap_or_default(),
                );
                keys.scan(&mut |key| {
                    held.push(key);
                    Ok(())
                })?;
                let (body, distinct) = sorted(&held, base, observed.rows)?;
                observed.distinct = distinct;
                Self { body, observed }
            }
        })
    }

    /// Which form this map took.
    #[must_use]
    pub fn form(&self) -> Form {
        match self.body {
            Body::Identity { .. } => Form::Identity,
            Body::Dense { .. } => Form::Dense,
            Body::Sorted { .. } => Form::Sorted,
            Body::Permuted { .. } => Form::Permuted,
        }
    }

    /// The smallest key and how many key values from it the map spans, when the keys are compact.
    ///
    /// The identity and dense forms are the two that exist because the keys fill most of a range,
    /// at most one hole in [`DENSE_THRESHOLD`] values, so a bitmap over that range is at most that
    /// many bits a parent row. That is what lets a join test a child's key against a set of parents
    /// with one subtraction and one bit, and with no link at all. The permuted form spans the same
    /// range the dense one does, since the span is about the keys and not about where their rows
    /// are. The sorted form is the one for keys spread over a range too wide for that, and answers
    /// `None`.
    #[must_use]
    pub fn span(&self) -> Option<(i128, u64)> {
        match self.body {
            Body::Identity { base, count } => Some((base, count)),
            Body::Dense { base, range, .. } | Body::Permuted { base, range, .. } => {
                Some((base, range))
            }
            Body::Sorted { .. } => None,
        }
    }

    /// What the build saw, which is the cardinality verification.
    #[must_use]
    pub fn observed(&self) -> &Observed {
        &self.observed
    }

    /// The value every stored key is an offset from, which is the smallest key.
    pub(crate) fn base(&self) -> i128 {
        match &self.body {
            Body::Identity { base, .. }
            | Body::Dense { base, .. }
            | Body::Sorted { base, .. }
            | Body::Permuted { base, .. } => *base,
        }
    }

    /// Appends the form's own bytes, after the header that `wire` has already written.
    ///
    /// Nothing here is stored that the header and the form together derive. The identity form
    /// writes nothing at all, because its count is the header's row count, which is section 3.3's
    /// "no extents beyond the header" in code rather than in prose.
    pub(crate) fn write_body(&self, out: &mut Vec<u8>) -> Result<()> {
        match &self.body {
            Body::Identity { .. } => Ok(()),
            Body::Dense { range, bits, rank, .. } => {
                out.extend_from_slice(&range.to_le_bytes());
                for word in bits {
                    out.extend_from_slice(&word.to_le_bytes());
                }
                rank.write(out);
                Ok(())
            }
            Body::Sorted { key_width, keys, rid_width, perm, .. } => {
                // The widths are a byte each, and a width past sixty four is a width no `u64` key
                // offset can have taken, so it is a torn header rather than a wide key.
                let widths = [*key_width, *rid_width];
                for width in widths {
                    let width = u8::try_from(width)
                        .map_err(|_| malformed("a sorted key map's width does not fit a byte"))?;
                    out.push(width);
                }
                out.extend_from_slice(keys);
                out.extend_from_slice(perm);
                Ok(())
            }
            Body::Permuted { range, bits, rank, rid_width, perm, .. } => {
                // The dense form's bytes and then the permutation, so that everything up to the
                // rank index reads the way the dense form's does. The rank index's length follows
                // from the range, which is how a reader finds where the permutation starts.
                out.extend_from_slice(&range.to_le_bytes());
                for word in bits {
                    out.extend_from_slice(&word.to_le_bytes());
                }
                rank.write(out);
                let width = u8::try_from(*rid_width)
                    .map_err(|_| malformed("a permuted key map's width does not fit a byte"))?;
                out.push(width);
                out.extend_from_slice(perm);
                Ok(())
            }
        }
    }

    /// Reads back what [`KeyMap::write_body`] wrote, and fills in the maximum key.
    ///
    /// The maximum is not in the header because each form derives it: identity from its count,
    /// dense from its range, sorted from its last stored key. That is the whole reason this takes
    /// [`Observed`] and returns a map rather than taking a finished one.
    ///
    /// # Errors
    ///
    /// If the body is not exactly the length its header implies. Exactly, not at least: a body
    /// longer than its form needs means the header and the body disagree about which form this is,
    /// and the safe reading of a disagreement is neither of them.
    pub(crate) fn read_body(
        form: Form,
        base: i128,
        mut observed: Observed,
        body: &[u8],
    ) -> Result<Self> {
        match form {
            Form::Identity => {
                if !body.is_empty() {
                    return Err(malformed("an identity key map has no body"));
                }
                if observed.rows > 0 {
                    observed.max = Some(
                        base.checked_add(i128::from(observed.rows) - 1)
                            .ok_or_else(|| malformed("an identity key map's range overflows"))?,
                    );
                }
                Ok(Self { body: Body::Identity { base, count: observed.rows }, observed })
            }
            Form::Dense => {
                let Some(head) = body.get(..size_of::<u64>()) else {
                    return Err(malformed("a dense key map has no range"));
                };
                let range = u64::from_le_bytes(head.try_into().expect("eight bytes"));
                let Ok(range_usize) = usize::try_from(range) else {
                    return Err(malformed("a dense key map's range does not fit this machine"));
                };
                let words = range_usize.div_ceil(64);
                let bitmap = words * size_of::<u64>();
                let rest = &body[size_of::<u64>()..];
                if rest.len() < bitmap {
                    return Err(malformed("a dense key map's bitmap is shorter than its range"));
                }
                let bits: Vec<u64> = rest[..bitmap]
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|word| u64::from_le_bytes(*word))
                    .collect();
                let rank = Rank::read(&rest[bitmap..], &bits)?;
                observed.max = Some(
                    base.checked_add(i128::from(range) - 1)
                        .ok_or_else(|| malformed("a dense key map's range overflows"))?,
                );
                Ok(Self { body: Body::Dense { base, range, bits, rank }, observed })
            }
            Form::Sorted => {
                if body.len() < 2 {
                    return Err(malformed("a sorted key map has no widths"));
                }
                let key_width = usize::from(body[0]);
                let rid_width = usize::from(body[1]);
                if key_width == 0 || key_width > 64 || rid_width == 0 || rid_width > 64 {
                    return Err(malformed("a sorted key map's width is not one a u64 can take"));
                }
                let count = observed.rows;
                let Ok(count_usize) = usize::try_from(count) else {
                    return Err(malformed(
                        "a sorted key map holds more keys than this machine can",
                    ));
                };
                let key_bytes = (count_usize * key_width).div_ceil(8);
                let perm_bytes = (count_usize * rid_width).div_ceil(8);
                let rest = &body[2..];
                if rest.len() != key_bytes + perm_bytes {
                    return Err(malformed(
                        "a sorted key map's arrays are not the size its widths and count imply",
                    ));
                }
                let keys = rest[..key_bytes].to_vec();
                let perm = rest[key_bytes..].to_vec();
                if count > 0 {
                    let largest = bitpack::tail_at(&keys, key_width, count_usize - 1)?;
                    observed.max =
                        Some(base.checked_add(i128::from(largest)).ok_or_else(|| {
                            malformed("a sorted key map's largest key overflows")
                        })?);
                }
                Ok(Self {
                    body: Body::Sorted { base, key_width, keys, rid_width, perm, count },
                    observed,
                })
            }
            Form::Permuted => {
                let Some(head) = body.get(..size_of::<u64>()) else {
                    return Err(malformed("a permuted key map has no range"));
                };
                let range = u64::from_le_bytes(head.try_into().expect("eight bytes"));
                let Ok(range_usize) = usize::try_from(range) else {
                    return Err(malformed("a permuted key map's range does not fit this machine"));
                };
                let words = range_usize.div_ceil(64);
                let bitmap = words * size_of::<u64>();
                let (blocks, superblocks) = Rank::shape(words);
                let ranks = superblocks * size_of::<u32>() + blocks * size_of::<u16>();
                let rest = &body[size_of::<u64>()..];
                if rest.len() < bitmap + ranks + 1 {
                    return Err(malformed("a permuted key map is shorter than its range implies"));
                }
                let bits: Vec<u64> = rest[..bitmap]
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|word| u64::from_le_bytes(*word))
                    .collect();
                let rank = Rank::read(&rest[bitmap..bitmap + ranks], &bits)?;
                let rid_width = usize::from(rest[bitmap + ranks]);
                if rid_width == 0 || rid_width > 64 {
                    return Err(malformed("a permuted key map's width is not one a u64 can take"));
                }
                let Ok(count) = usize::try_from(observed.rows) else {
                    return Err(malformed(
                        "a permuted key map holds more keys than this machine can",
                    ));
                };
                let perm = rest[bitmap + ranks + 1..].to_vec();
                if perm.len() != (count * rid_width).div_ceil(8) {
                    return Err(malformed(
                        "a permuted key map's permutation is not the size its width and count imply",
                    ));
                }
                observed.max = Some(
                    base.checked_add(i128::from(range) - 1)
                        .ok_or_else(|| malformed("a permuted key map's range overflows"))?,
                );
                Ok(Self {
                    body: Body::Permuted { base, range, bits, rank, rid_width, perm },
                    observed,
                })
            }
        }
    }

    /// Keys this map resolves.
    #[must_use]
    pub fn len(&self) -> u64 {
        match &self.body {
            Body::Identity { count, .. } | Body::Sorted { count, .. } => *count,
            Body::Dense { .. } | Body::Permuted { .. } => self.observed.rows,
        }
    }

    /// Whether this map resolves nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes this map holds resident, for the budget of section 3.7 and the cache of section 4.4.
    ///
    /// Identity is twenty four bytes and says so, which is the number that makes the budget
    /// livable on TPC-H.
    #[must_use]
    pub fn bytes(&self) -> usize {
        match &self.body {
            Body::Identity { .. } => size_of::<i128>() + size_of::<u64>(),
            Body::Dense { bits, rank, .. } => bits.len() * size_of::<u64>() + rank.bytes(),
            Body::Sorted { keys, perm, .. } => keys.len() + perm.len(),
            Body::Permuted { bits, rank, perm, .. } => {
                bits.len() * size_of::<u64>() + rank.bytes() + perm.len()
            }
        }
    }

    /// The rows holding a set of keys, where the set is a bitmap over [`Self::span`] and so is the
    /// answer, over the `rows` rows of the parent.
    ///
    /// The same set a [`Self::lookup`] of every key in `held` would make, for a walk over the words
    /// of the two bitmaps. A key's row in the dense form is how many keys come before it, and the
    /// word it sits in says that for all sixty four of its keys at once, so the rank is kept running
    /// from one word to the next rather than asked for again at each key. On TPC-H q21 a lookup
    /// at a time was about 185 instructions a key, over the three hundred thousand `lineitem` keys
    /// two of its joins hold, and the join had already set the same keys in a bitmap over the span.
    ///
    /// `held` may run one word past the span, which is what a join's bitmap does so that a key past
    /// the end has somewhere to land, as long as nothing past the span is set.
    ///
    /// `None` when the map has no span, which is the sorted form, or when `held` holds a key the
    /// map does not, which a caller answers the way it would a key [`Self::lookup`] did not find.
    ///
    /// # Errors
    ///
    /// If a bit packed payload is torn, the same as [`Self::lookup`].
    pub fn rows_of_span(&self, held: &[u64], rows: u64) -> Result<Option<Vec<u64>>> {
        let words = usize::try_from(rows.div_ceil(64)).unwrap_or(usize::MAX);
        let Some((_, range)) = self.span() else { return Ok(None) };
        let span_words = usize::try_from(range.div_ceil(64)).unwrap_or(usize::MAX);
        if held.len() < span_words || held[span_words..].iter().any(|&word| word != 0) {
            return Ok(None);
        }
        // The last word of the span is only partly inside it.
        if range % 64 != 0 && held[span_words - 1] >> (range % 64) != 0 {
            return Ok(None);
        }
        let held = &held[..span_words];
        let mut out = vec![0_u64; words];
        match &self.body {
            Body::Identity { count, .. } => {
                if *count > rows {
                    return Ok(None);
                }
                out[..span_words].copy_from_slice(held);
            }
            Body::Dense { bits, .. } => {
                if bits.len() < held.len() {
                    return Ok(None);
                }
                let mut before = 0_u64;
                for (&keys, &present) in held.iter().zip(bits) {
                    if keys & !present != 0 {
                        return Ok(None);
                    }
                    let mut left = keys;
                    while left != 0 {
                        let below = (1_u64 << left.trailing_zeros()) - 1;
                        let rid = before + u64::from((present & below).count_ones());
                        let Some(word) = out.get_mut((rid / 64) as usize) else { return Ok(None) };
                        *word |= 1 << (rid % 64);
                        left &= left - 1;
                    }
                    before += u64::from(present.count_ones());
                }
            }
            Body::Permuted { bits, rid_width, perm, .. } => {
                if bits.len() < held.len() {
                    return Ok(None);
                }
                let mut before = 0_u64;
                for (&keys, &present) in held.iter().zip(bits) {
                    if keys & !present != 0 {
                        return Ok(None);
                    }
                    let mut left = keys;
                    while left != 0 {
                        let below = (1_u64 << left.trailing_zeros()) - 1;
                        let place = before + u64::from((present & below).count_ones());
                        #[expect(
                            clippy::cast_possible_truncation,
                            reason = "a rank is below the key count, which the build checked fits a usize"
                        )]
                        let rid = bitpack::tail_at(perm, *rid_width, place as usize)?;
                        let Some(word) = out.get_mut((rid / 64) as usize) else { return Ok(None) };
                        *word |= 1 << (rid % 64);
                        left &= left - 1;
                    }
                    before += u64::from(present.count_ones());
                }
            }
            Body::Sorted { .. } => return Ok(None),
        }
        if !rows.is_multiple_of(64) && out.last().is_some_and(|&word| word >> (rows % 64) != 0) {
            return Ok(None);
        }
        Ok(Some(out))
    }

    /// The key each of `rids` holds, the other way round from [`Self::lookup`].
    ///
    /// A child row of a link holds the key of the parent the link points it at, so a scan that
    /// knows which parent each of its rows has can take its key column from here rather than decode
    /// it. Only the identity and dense forms answer, since a row's key there is its place among the
    /// keys present, and the rows a link hands over usually rise, so each search starts at the word
    /// the last one ended in. They also come in runs, one parent row for each of its children, and
    /// a row the same as the last one is the key the last one had. `None` for the other forms, or
    /// for a row past the last.
    #[must_use]
    pub fn keys_at(&self, rids: &[Rid]) -> Option<Vec<i128>> {
        match &self.body {
            Body::Identity { base, count } => {
                rids.iter().map(|&rid| (rid < *count).then(|| base + i128::from(rid))).collect()
            }
            Body::Dense { base, bits, rank, .. } => {
                let mut out = Vec::with_capacity(rids.len());
                let mut last = (0, 0);
                let mut held = None;
                for &rid in rids {
                    if let Some((was, key)) = held
                        && was == rid
                    {
                        out.push(key);
                        continue;
                    }
                    let from = if rid >= last.0 { last.1 } else { 0 };
                    let (word, within) = rank.word_holding(rid, from)?;
                    let offset = word as u64 * 64 + u64::from(nth_set(bits[word], within));
                    let key = base + i128::from(offset);
                    out.push(key);
                    last = (rid, word);
                    held = Some((rid, key));
                }
                Some(out)
            }
            Body::Sorted { .. } | Body::Permuted { .. } => None,
        }
    }

    /// [`Self::lookup`] for a block of keys, with [`NO_PARENT`] for a key that has no row.
    ///
    /// An identity map is a subtraction and a range check a key, which is every key column of the
    /// IMDb load. Called a key at a time through `i128` and a `Result` it was about a sixth of the
    /// CPU the links of a JOB checkpoint took.
    ///
    /// # Errors
    ///
    /// As [`Self::lookup`].
    pub fn lookup_block(&self, keys: &[i64], out: &mut [Rid]) -> Result<()> {
        if let Body::Identity { base, count } = &self.body {
            for (key, out) in keys.iter().zip(out) {
                let offset = u64::try_from(i128::from(*key) - base).ok();
                *out = offset.filter(|rid| rid < count).unwrap_or(NO_PARENT);
            }
            return Ok(());
        }
        for (key, out) in keys.iter().zip(out) {
            *out = self.lookup(i128::from(*key))?.unwrap_or(NO_PARENT);
        }
        Ok(())
    }

    /// The `rid` of the row holding this key, or `None` when no row holds it.
    ///
    /// `None` is the ordinary answer and not an exceptional one: a child key with no matching
    /// parent is what section 2.4 reserves *no parent* for, and a null child key never reaches
    /// here at all.
    ///
    /// # Errors
    ///
    /// If a bit packed payload is torn, which is a corrupt section rather than a missing key.
    pub fn lookup(&self, key: i128) -> Result<Option<Rid>> {
        match &self.body {
            Body::Identity { base, count } => {
                let Some(offset) = key.checked_sub(*base) else {
                    return Ok(None);
                };
                match u64::try_from(offset) {
                    Ok(rid) if rid < *count => Ok(Some(rid)),
                    _ => Ok(None),
                }
            }
            Body::Dense { base, range, bits, rank } => {
                let Some(offset) = key.checked_sub(*base) else {
                    return Ok(None);
                };
                let Ok(offset) = u64::try_from(offset) else {
                    return Ok(None);
                };
                if offset >= *range {
                    return Ok(None);
                }
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "the build checked the range fits a usize"
                )]
                let at = offset as usize;
                if bits[at / 64] >> (at % 64) & 1 == 0 {
                    return Ok(None);
                }
                Ok(Some(rank.rank(bits, at)))
            }
            Body::Permuted { base, range, bits, rank, rid_width, perm } => {
                let Some(offset) = key.checked_sub(*base) else {
                    return Ok(None);
                };
                let Ok(offset) = u64::try_from(offset) else {
                    return Ok(None);
                };
                if offset >= *range {
                    return Ok(None);
                }
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "the build checked the range fits a usize"
                )]
                let at = offset as usize;
                if bits[at / 64] >> (at % 64) & 1 == 0 {
                    return Ok(None);
                }
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "a rank is below the key count, which the build checked fits a usize"
                )]
                let place = rank.rank(bits, at) as usize;
                Ok(Some(bitpack::tail_at(perm, *rid_width, place)?))
            }
            Body::Sorted { base, key_width, keys, rid_width, perm, count } => {
                let Some(offset) = key.checked_sub(*base) else {
                    return Ok(None);
                };
                let Ok(wanted) = u64::try_from(offset) else {
                    return Ok(None);
                };
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "the build refused a column wider than a usize of rows"
                )]
                let len = *count as usize;
                // A plain binary search over the packed keys. Branchless in the sense that matters
                // here, which is that the comparison drives an index rather than a branch to a
                // different loop, and every probe is one `tail_at` rather than a decode of the
                // block around it.
                let mut low = 0_usize;
                let mut high = len;
                while low < high {
                    let mid = low + (high - low) / 2;
                    let at = bitpack::tail_at(keys, *key_width, mid)?;
                    if at < wanted {
                        low = mid + 1;
                    } else {
                        high = mid;
                    }
                }
                if low >= len || bitpack::tail_at(keys, *key_width, low)? != wanted {
                    return Ok(None);
                }
                Ok(Some(bitpack::tail_at(perm, *rid_width, low)?))
            }
        }
    }
}

/// A parent key column that can be read more than once, in `rid` order.
///
/// The build wants two passes over a column it does not want to hold, so this is what it reads
/// instead of a slice: something that can be asked to produce the column again. A file can do that
/// for the price of a read, and the second read is against pages the first one just warmed.
///
/// Values arrive as `Option<i128>`, with `None` for a null. A string key arrives as its dictionary
/// code rather than as text, per section 2.2, which is why one integer signature covers every key
/// type rudb has.
pub trait Keys {
    /// Calls `each` once per row of the column, in `rid` order.
    ///
    /// # Errors
    ///
    /// If the column cannot be read, or if `each` fails, which stops the scan rather than
    /// continuing past a value that could not be used.
    fn scan(&self, each: &mut dyn FnMut(Option<i128>) -> Result<()>) -> Result<()>;
}

impl Keys for [Option<i128>] {
    fn scan(&self, each: &mut dyn FnMut(Option<i128>) -> Result<()>) -> Result<()> {
        for key in self {
            each(*key)?;
        }
        Ok(())
    }
}

/// Which form the build chose, decided once and carried out twice.
///
/// Separating the decision from the filling is what lets [`KeyMap::build`] and
/// [`KeyMap::build_from`] be the same build. A second copy of these three conditions is a second
/// place for the positional guard below to be got wrong.
enum Plan {
    Empty,
    Identity { base: i128, count: u64 },
    Dense { base: i128, range: u64 },
    Permuted { base: i128, range: u64 },
    Sorted { base: i128 },
}

/// Picks the cheapest form that is correct for what the column turned out to hold.
fn plan(observed: &Observed) -> Result<Plan> {
    // A column with no keys in it at all is an identity map over nothing. It is worth having rather
    // than refusing, because an empty parent table is a legal table and a join against it returns
    // no rows rather than failing.
    if observed.rows == 0 {
        return Ok(Plan::Empty);
    }
    let (Some(min), Some(max)) = (observed.min, observed.max) else {
        // A non-zero row count guarantees both, so this is unreachable. It is an error rather than
        // an `expect` because a key map that panicked on its own bookkeeping would take down a
        // query that section 3.1 promises can always be answered without it.
        return Err(malformed("a column with keys in it reported no minimum"));
    };
    let range = range_of(min, max)?;

    // Both of the cheap forms answer with a *count of keys below the value*, and both are correct
    // only where that count is the `rid`. It is the `rid` when the column is ascending and holds no
    // nulls, and it is not otherwise: a null earlier in the column, or a value out of order, shifts
    // every row after it. Getting this wrong would not fail, it would resolve every key to a
    // neighbour of the right row, which is the one failure mode section 3.1 does not catch for
    // free. So the guard is shared and stated once.
    let positional = observed.distinct && observed.sorted && observed.nulls == 0;

    if positional && range == observed.rows {
        // Identity needs more than positional: it needs the values to be exactly the positions,
        // which on a distinct ascending column is the range equalling the row count. The check is
        // subtraction rather than a walk because the walk already happened in the observation.
        return Ok(Plan::Identity { base: min, count: observed.rows });
    }

    // The bitmap is over the value range, so a range that does not fit a `usize` cannot be one
    // however dense it is.
    let compact = usize::try_from(range).is_ok() && range / observed.rows < DENSE_THRESHOLD;
    if positional && compact {
        return Ok(Plan::Dense { base: min, range });
    }

    // The same keys stored in another order, or with nulls between them. A rank is no longer a
    // `rid`, so a permutation turns one into the other. It is half the sorted form or less whenever
    // the range is compact, because the bitmap costs under a byte a key where the sorted keys cost
    // their width, and it answers with a bit test and a rank rather than a search. A column the
    // first scan already saw repeat never gets here.
    if observed.distinct && compact {
        return Ok(Plan::Permuted { base: min, range });
    }

    Ok(Plan::Sorted { base: min })
}

/// The four facts section 3.3 says the build records, accumulated one value at a time.
///
/// One value at a time rather than one column at a time so that the pass can be driven by a scan
/// of a file as easily as by a slice. See [`KeyMap::build_from`] for why that matters.
struct Observer {
    observed: Observed,
    previous: Option<i128>,
}

impl Observer {
    fn new() -> Self {
        Self {
            observed: Observed {
                rows: 0,
                nulls: 0,
                distinct: true,
                sorted: true,
                min: None,
                max: None,
            },
            previous: None,
        }
    }

    // Distinctness on a column that is not sorted cannot be settled in one pass without a set, so
    // this settles it for the sorted case and leaves the unsorted case to the sort that the sorted
    // form does anyway. That is why `distinct` is fixed up in `sorted` below rather than being
    // final here, and it is worth the awkwardness: the common case on real keys is ascending, and a
    // hash set over fifteen million rows to discover what adjacency already proves is the build
    // cost this avoids.
    fn push(&mut self, key: Option<i128>) {
        let Some(key) = key else {
            self.observed.nulls += 1;
            return;
        };
        self.observed.rows += 1;
        self.observed.min = Some(self.observed.min.map_or(key, |held| held.min(key)));
        self.observed.max = Some(self.observed.max.map_or(key, |held| held.max(key)));
        if let Some(previous) = self.previous {
            if key < previous {
                self.observed.sorted = false;
            } else if key == previous {
                self.observed.distinct = false;
            }
        }
        self.previous = Some(key);
    }
}

/// One pass over the column, recording the four facts section 3.3 says the build records.
fn observe(keys: &[Option<i128>]) -> Observed {
    let mut observer = Observer::new();
    for key in keys {
        observer.push(*key);
    }
    observer.observed
}

/// How many distinct values lie between `min` and `max` inclusive.
///
/// The arithmetic is in `u128` and not `i128` because a column holding both `i128::MIN` and
/// `i128::MAX` has a range of `2^128`, and `max - min` on an `i128` for that column is an overflow
/// rather than a number. A `HUGEINT` key column spanning more than a `u64` of values is pathological
/// but legal, so it gets an error naming what happened rather than a panic in a build: the caller
/// records the relationship as not built, exactly as it does for one that does not fit the budget.
///
/// `max >= min` always holds here, so the wrapping subtraction is exact in `u128`.
fn range_of(min: i128, max: i128) -> Result<u64> {
    let span = max.wrapping_sub(min) as u128;
    u64::try_from(span)
        .ok()
        .and_then(|span| span.checked_add(1))
        .ok_or_else(|| malformed("the key column spans more than a u64 of values"))
}

/// The offset a key takes from the base.
///
/// `range_of` bounded the span to a `u64` before either form that uses this was chosen, so the
/// subtraction cannot overflow and the offset cannot exceed a `u64`. Both are checked anyway: this
/// is the one arithmetic in the crate whose silent failure would resolve keys to the wrong rows.
fn offset_of(key: i128, base: i128) -> Result<u64> {
    let offset = key
        .checked_sub(base)
        .ok_or_else(|| malformed("a key is further from the base than an i128 holds"))?;
    u64::try_from(offset)
        .map_err(|_| malformed("a key is below the base or further from it than a u64 holds"))
}

/// Builds the bitmap form one key at a time.
///
/// The caller guarantees the column is distinct, ascending and null free, which is what makes a
/// rank equal to a `rid`. The assertion restates it where the correctness depends on it rather than
/// where the decision was made.
struct DenseBits {
    base: i128,
    range: u64,
    bits: Vec<u64>,
    previous: Option<i128>,
}

impl DenseBits {
    fn new(base: i128, range: u64) -> Self {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the caller checked the range fits a usize"
        )]
        let range_usize = range as usize;
        Self { base, range, bits: vec![0_u64; range_usize.div_ceil(64)], previous: None }
    }

    fn push(&mut self, key: i128) -> Result<()> {
        debug_assert!(
            self.previous.is_none_or(|held| key > held),
            "the bitmap form needs a distinct ascending column, because a rank is a count of keys below a value and that is a rid only there"
        );
        self.previous = Some(key);
        let offset = offset_of(key, self.base)?;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the caller checked the range fits a usize and the offset is inside it"
        )]
        let at = offset as usize;
        self.bits[at / 64] |= 1 << (at % 64);
        Ok(())
    }

    fn finish(self) -> Body {
        let rank = Rank::build(&self.bits);
        Body::Dense { base: self.base, range: self.range, bits: self.bits, rank }
    }

    /// Sets a key's bit in any order, and says whether it was clear, which is false for a repeat.
    fn mark(&mut self, key: i128) -> Result<bool> {
        let offset = offset_of(key, self.base)?;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the caller checked the range fits a usize and the offset is inside it"
        )]
        let at = offset as usize;
        let bit = 1 << (at % 64);
        let fresh = self.bits[at / 64] & bit == 0;
        self.bits[at / 64] |= bit;
        Ok(fresh)
    }
}

/// The permutation of the permuted form, filled one row at a time in any order.
///
/// Built over a finished bitmap, so a key's place is its rank and every place is filled exactly
/// once. Held as `u64` a key while it fills, which is the one allocation the build makes that the
/// map does not keep, and packed at the end to the width the row count needs.
struct Permutation {
    base: i128,
    range: u64,
    bits: Vec<u64>,
    rank: Rank,
    places: Vec<u64>,
    rid_width: usize,
}

impl Permutation {
    /// Takes the finished bitmap, and `rows`, which is how many rows the column has, nulls included.
    fn new(bits: DenseBits, rows: usize) -> Result<Self> {
        let keys: u64 = bits.bits.iter().map(|word| u64::from(word.count_ones())).sum();
        let keys =
            usize::try_from(keys).map_err(|_| malformed("too many keys for this machine"))?;
        let largest = u64::try_from(rows.saturating_sub(1))
            .map_err(|_| malformed("the column is too long for a rid"))?;
        let rank = Rank::build(&bits.bits);
        Ok(Self {
            base: bits.base,
            range: bits.range,
            bits: bits.bits,
            rank,
            places: vec![0; keys],
            rid_width: width_for(largest),
        })
    }

    /// Puts `rid` at the place of `key`, which the bitmap already holds.
    fn place(&mut self, key: i128, rid: usize) -> Result<()> {
        let offset = offset_of(key, self.base)?;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the offset is inside a range the caller checked fits a usize"
        )]
        let at = offset as usize;
        let place = usize::try_from(self.rank.rank(&self.bits, at))
            .map_err(|_| malformed("a rank past what this machine can index"))?;
        let rid = u64::try_from(rid).map_err(|_| malformed("the column is too long for a rid"))?;
        let slot = self
            .places
            .get_mut(place)
            .ok_or_else(|| malformed("a key was placed that the bitmap does not hold"))?;
        *slot = rid;
        Ok(())
    }

    fn finish(self) -> Result<Body> {
        let mut perm = Vec::new();
        // The linear packer for the reason [`sorted`] gives: a parent key column is longer than a
        // tail, and `tail_at` reads either layout.
        bitpack::pack_linear(&self.places, self.rid_width, &mut perm)?;
        Ok(Body::Permuted {
            base: self.base,
            range: self.range,
            bits: self.bits,
            rank: self.rank,
            rid_width: self.rid_width,
            perm,
        })
    }
}

fn dense(keys: &[Option<i128>], base: i128, range: u64) -> Result<Body> {
    let mut bits = DenseBits::new(base, range);
    for key in keys.iter().flatten() {
        bits.push(*key)?;
    }
    Ok(bits.finish())
}

/// Builds the general form, and settles distinctness on the way.
///
/// Returns the body and whether every key was distinct. The second is not a courtesy: the sort this
/// form performs is the only place a duplicate that is not adjacent in the column can be seen, and
/// section 2.3 needs that answer to decide whether a link may be built at all.
fn sorted(keys: &[Option<i128>], base: i128, rows: u64) -> Result<(Body, bool)> {
    let mut pairs: Vec<(u64, u64)> = Vec::with_capacity(keys.len());
    for (rid, key) in keys.iter().enumerate() {
        let Some(key) = *key else { continue };
        let offset = offset_of(key, base)?;
        let rid = u64::try_from(rid).map_err(|_| malformed("the column is too long for a rid"))?;
        pairs.push((offset, rid));
    }
    // Sorted by key, then by rid so that a duplicated key resolves to its first row rather than to
    // whichever one the sort happened to leave first. A duplicated key means no link gets built, so
    // this only decides what a map nobody should be using returns, and deciding it anyway is what
    // keeps a test of this form reproducible.
    pairs.sort_unstable();
    let distinct = pairs.windows(2).all(|pair| pair[0].0 != pair[1].0);
    debug_assert_eq!(
        u64::try_from(pairs.len()).ok(),
        Some(rows),
        "the pair list is the non-null column"
    );
    let key_width = width_for(pairs.last().map_or(0, |pair| pair.0));
    let rows_width = u64::try_from(keys.len().saturating_sub(1))
        .map_err(|_| malformed("the column is too long for a rid"))?;
    let rid_width = width_for(rows_width);
    let mut key_bytes = Vec::new();
    let mut rid_bytes = Vec::new();
    let key_values: Vec<u64> = pairs.iter().map(|pair| pair.0).collect();
    let rid_values: Vec<u64> = pairs.iter().map(|pair| pair.1).collect();
    // The linear packer and not the tail one, because a tail is bounded at a thousand values and a
    // parent key column is not. The layout is the same and `tail_at` reads either.
    bitpack::pack_linear(&key_values, key_width, &mut key_bytes)?;
    bitpack::pack_linear(&rid_values, rid_width, &mut rid_bytes)?;
    Ok((
        Body::Sorted { base, key_width, keys: key_bytes, rid_width, perm: rid_bytes, count: rows },
        distinct,
    ))
}

/// Bits needed to hold every value up to and including `largest`.
///
/// One rather than zero for a largest of zero, because a width of zero is a packed payload with no
/// bytes in it and `tail_at` on one of those has nothing to return. A column of a single key is a
/// real column.
fn width_for(largest: u64) -> usize {
    let bits = u64::BITS - largest.leading_zeros();
    bits.max(1) as usize
}

fn malformed(message: impl Into<String>) -> Error {
    Error::corrupted(format!("invalid key map: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(values: &[i128]) -> Vec<Option<i128>> {
        values.iter().copied().map(Some).collect()
    }

    /// Every key in the column resolves to the row that holds it, whatever form was chosen.
    fn resolves(column: &[Option<i128>], map: &KeyMap) {
        for (rid, key) in column.iter().enumerate() {
            let Some(key) = *key else { continue };
            let found = map.lookup(key).expect("lookup").expect("a key in the column resolves");
            assert_eq!(found, rid as u64, "key {key} resolved to {found} rather than {rid}");
        }
    }

    #[test]
    fn a_sequence_from_one_is_the_identity_form_and_stores_two_numbers() {
        // TPC-H's `region`, `nation`, `supplier`, `customer`, `part` and `orders` all land here,
        // which is the case the whole budget in section 3.7 depends on.
        let column = keys(&(1..=1000).collect::<Vec<i128>>());
        let map = KeyMap::build(&column).expect("build");
        assert_eq!(map.form(), Form::Identity);
        assert_eq!(map.bytes(), 24, "section 4.2 says identity is twenty four bytes");
        assert_eq!(map.len(), 1000);
        resolves(&column, &map);
        assert_eq!(map.lookup(0).expect("lookup"), None, "below the base");
        assert_eq!(map.lookup(1001).expect("lookup"), None, "past the end");
        assert_eq!(map.span(), Some((1, 1000)));
    }

    #[test]
    fn a_block_of_keys_finds_what_a_key_at_a_time_finds() {
        let identity = keys(&(-5..995).collect::<Vec<i128>>());
        let dense = keys(&(0..3000).filter(|value| value % 3 != 1).collect::<Vec<i128>>());
        let mut shuffled = (0..3000).filter(|value| value % 3 != 1).collect::<Vec<i128>>();
        shuffled.sort_by_key(|value| (value * 7919) % 3001);
        let permuted = keys(&shuffled);
        let asked = (-20..3020).chain([i64::MIN, i64::MAX, -6, 995]).collect::<Vec<i64>>();
        for (column, form) in
            [(identity, Form::Identity), (dense, Form::Dense), (permuted, Form::Permuted)]
        {
            let map = KeyMap::build(&column).expect("build");
            assert_eq!(map.form(), form);
            let mut found = vec![0; asked.len()];
            map.lookup_block(&asked, &mut found).expect("block");
            for (key, rid) in asked.iter().zip(&found) {
                let one = map.lookup(i128::from(*key)).expect("lookup").unwrap_or(NO_PARENT);
                assert_eq!(*rid, one, "key {key} of the {form:?} form");
            }
        }
    }

    #[test]
    fn the_rows_of_a_span_are_the_rows_a_lookup_of_each_key_finds() {
        let identity = keys(&(5..1005).collect::<Vec<i128>>());
        let dense = keys(&(0..3000).filter(|value| value % 3 != 1).collect::<Vec<i128>>());
        let mut shuffled = (0..3000).filter(|value| value % 3 != 1).collect::<Vec<i128>>();
        shuffled.sort_by_key(|value| (value * 7919) % 3001);
        let permuted = keys(&shuffled);
        for (column, form) in
            [(identity, Form::Identity), (dense, Form::Dense), (permuted, Form::Permuted)]
        {
            let map = KeyMap::build(&column).expect("build");
            assert_eq!(map.form(), form);
            let rows = column.len() as u64;
            let (base, range) = map.span().expect("a span");
            let mut held = vec![0_u64; (range / 64 + 1) as usize];
            let mut wanted = vec![0_u64; rows.div_ceil(64) as usize];
            for key in column.iter().flatten().filter(|key| *key % 5 == 0 || *key % 7 == 3) {
                let offset = (key - base) as u64;
                held[(offset / 64) as usize] |= 1 << (offset % 64);
                let rid = map.lookup(*key).expect("lookup").expect("a key in the column");
                wanted[(rid / 64) as usize] |= 1 << (rid % 64);
            }
            assert_eq!(map.rows_of_span(&held, rows).expect("rows"), Some(wanted), "{form:?}");
            if form != Form::Identity {
                // A key inside the span that no row holds.
                let missing = (0..range).find(|offset| {
                    map.lookup(base + i128::from(*offset)).expect("lookup").is_none()
                });
                let offset = missing.expect("a hole in the span");
                held[(offset / 64) as usize] |= 1 << (offset % 64);
                assert_eq!(map.rows_of_span(&held, rows).expect("rows"), None, "{form:?}");
            }
            let last = held.len() - 1;
            held[last] |= 1 << 63;
            assert_eq!(map.rows_of_span(&held, rows).expect("rows"), None, "past the span");
        }
        let sorted = keys(&(0..1000).map(|value| value * 1000).collect::<Vec<i128>>());
        let map = KeyMap::build(&sorted).expect("build");
        assert_eq!(map.rows_of_span(&[0; 4], 1000).expect("rows"), None, "no span");
    }

    #[test]
    fn the_keys_at_some_rows_are_the_keys_those_rows_hold() {
        let identity = keys(&(5..1005).collect::<Vec<i128>>());
        let dense = keys(&(0..3000).filter(|value| value % 3 != 1).collect::<Vec<i128>>());
        for (column, form) in [(identity, Form::Identity), (dense, Form::Dense)] {
            let map = KeyMap::build(&column).expect("build");
            assert_eq!(map.form(), form);
            let rows = column.len() as u64;
            // Rising with gaps, then falling back, then the last row.
            let mut rids: Vec<u64> =
                (0..rows).filter(|rid| rid % 7 == 2 || rid % 11 == 0).collect();
            rids.extend([3, 0, rows - 1]);
            let wanted: Vec<i128> =
                rids.iter().map(|&rid| column[rid as usize].expect("a key")).collect();
            assert_eq!(map.keys_at(&rids), Some(wanted), "{form:?}");
            assert_eq!(map.keys_at(&[rows]), None, "{form:?} past the last row");
        }
        let sorted = keys(&(0..1000).map(|value| value * 1000).collect::<Vec<i128>>());
        let map = KeyMap::build(&sorted).expect("build");
        assert_eq!(map.keys_at(&[0]), None, "the sorted form does not answer");
    }

    #[test]
    fn a_sequence_from_zero_is_also_the_identity_form() {
        let column = keys(&(0..64).collect::<Vec<i128>>());
        let map = KeyMap::build(&column).expect("build");
        assert_eq!(map.form(), Form::Identity);
        resolves(&column, &map);
    }

    #[test]
    fn a_sequence_with_a_gap_in_it_is_the_dense_form() {
        // Every other value over a range of two thousand, which is a density of one in two and
        // comfortably inside the threshold.
        let column = keys(&(0..1000).map(|value| value * 2).collect::<Vec<i128>>());
        let map = KeyMap::build(&column).expect("build");
        assert_eq!(map.form(), Form::Dense);
        resolves(&column, &map);
        assert_eq!(
            map.lookup(1).expect("lookup"),
            None,
            "a value in the range and not in the column"
        );
        assert_eq!(map.lookup(2001).expect("lookup"), None, "past the range");
        assert_eq!(map.span(), Some((0, 1999)), "from the smallest key to the largest");
    }

    #[test]
    fn a_range_too_sparse_for_a_bitmap_is_the_sorted_form() {
        // A thousand keys spread over a million, which is a density of one in a thousand: the
        // bitmap would be 125 KB to hold a thousand values and the sorted form is a few kilobytes.
        let column = keys(&(0..1000).map(|value| value * 1000).collect::<Vec<i128>>());
        let map = KeyMap::build(&column).expect("build");
        assert_eq!(map.form(), Form::Sorted);
        resolves(&column, &map);
        assert_eq!(map.lookup(500).expect("lookup"), None);
        assert_eq!(map.span(), None, "too sparse for a bitmap over the range");
    }

    #[test]
    fn the_sorted_form_is_not_bounded_by_a_packed_unit() {
        // The sorted form packs its keys and its permutation sequentially, and the sequential
        // packer a column uses is for the remainder past the last transposed unit, so it refuses a
        // thousand and twenty four values. A parent key column is sixty times that at SF1 and
        // fifteen thousand times it at SF10, so the form would exist only for toy tables. This is
        // the smallest column that would have hit it.
        let column = keys(&(0..5000).map(|value| (value * 7919) % 100_003).collect::<Vec<i128>>());
        let map = KeyMap::build(&column).expect("build");
        assert_eq!(map.form(), Form::Sorted);
        resolves(&column, &map);
    }

    #[test]
    fn keys_in_no_order_at_all_resolve_to_the_rows_that_hold_them() {
        // The case the permutation exists for. The column is not sorted, so the sorted form's
        // position is not the rid, and a map that confused the two would resolve every key to the
        // wrong row while looking exactly like a working map.
        let column = keys(&[500, 3, 9000, 12, 7, 88, 41, 6]);
        let map = KeyMap::build(&column).expect("build");
        assert_eq!(map.form(), Form::Sorted);
        resolves(&column, &map);
    }

    #[test]
    fn a_descending_column_dense_enough_for_a_bitmap_still_resolves_correctly() {
        // The trap in the dense form: a bitmap is in value order, so a rank is a position in value
        // order, and on a descending column that is not the rid. So it takes the permutation too.
        let column = keys(&(0..500).rev().collect::<Vec<i128>>());
        let map = KeyMap::build(&column).expect("build");
        assert_eq!(map.form(), Form::Permuted, "a descending column cannot take the bare bitmap");
        resolves(&column, &map);
    }

    #[test]
    fn nulls_are_not_keys_and_do_not_shift_the_rows_around_them() {
        // This column is distinct, ascending, and dense enough for a bitmap on the numbers alone:
        // three keys over a range of twenty one. It cannot have the bare one, because a rank counts
        // keys below a value and the nulls in between mean that count is not the row's position. A
        // map that took the bitmap here would resolve key 20 to row 1 and look entirely healthy
        // doing it, so it takes the permutation behind the bitmap.
        let column = vec![Some(10), None, Some(20), None, Some(30)];
        let map = KeyMap::build(&column).expect("build");
        assert_eq!(
            map.form(),
            Form::Permuted,
            "a null before a key shifts it out of the positional forms"
        );
        resolves(&column, &map);
        assert_eq!(map.observed().nulls, 2);
        assert_eq!(map.observed().rows, 3);
        assert_eq!(
            map.lookup(20).expect("lookup"),
            Some(2),
            "the rid is the position in the column"
        );
    }

    #[test]
    fn a_leading_null_keeps_an_otherwise_perfect_sequence_out_of_the_identity_form() {
        // The same trap on the form that would otherwise be free. Worth its own test because a
        // sequence from one is the case every TPC-H table hits, and the version of it with a null
        // in front is one `INSERT` away.
        let mut column = vec![None];
        column.extend((1..=1000).map(Some));
        let map = KeyMap::build(&column).expect("build");
        assert_ne!(map.form(), Form::Identity);
        resolves(&column, &map);
        assert_eq!(map.lookup(1).expect("lookup"), Some(1), "row zero is the null, not key one");
    }

    #[test]
    fn a_null_only_column_builds_and_resolves_nothing() {
        let column = vec![None, None, None];
        let map = KeyMap::build(&column).expect("build");
        assert!(map.is_empty());
        assert_eq!(map.observed().nulls, 3);
        assert_eq!(map.lookup(0).expect("lookup"), None);
    }

    #[test]
    fn an_empty_column_builds_and_resolves_nothing() {
        let map = KeyMap::build(&[]).expect("build");
        assert!(map.is_empty());
        assert_eq!(map.lookup(0).expect("lookup"), None);
        assert!(map.observed().usable_as_parent(), "an empty parent is unique, vacuously");
    }

    #[test]
    fn a_duplicated_key_is_reported_rather_than_resolved_to_one_of_its_rows() {
        // Section 2.3's verification. The map still builds, because the caller is the one that
        // decides what to do about it, and what it decides is to build no link.
        let column = keys(&[5, 7, 5, 9]);
        let map = KeyMap::build(&column).expect("build");
        assert!(!map.observed().distinct);
        assert!(!map.observed().usable_as_parent(), "a non-unique parent side takes no link");
    }

    #[test]
    fn a_column_that_arrives_with_its_repeats_together_is_not_sorted_into_a_map() {
        // The scan sees the repeat, so nothing is packed. What matters is the bytes: this is
        // `lineitem(l_orderkey)`, where the form that would have been chosen holds one packed key
        // and one packed permutation entry per row.
        let column = keys(&[1, 1, 2, 2, 2, 90_000, 90_000]);
        let map = KeyMap::build_from(&column[..]).expect("build");
        assert!(!map.observed().distinct);
        assert_eq!(map.observed().rows, 7, "the column was still counted");
        assert_eq!(map.observed().max, Some(90_000));
        assert_eq!(map.bytes(), KeyMap::build(&keys(&[])).expect("build").bytes());
        assert_eq!(map.lookup(2).expect("lookup"), None, "and it answers nothing, as it must");
    }

    #[test]
    fn a_single_key_column_resolves_it() {
        // The width of zero case: one key at the base is an offset of zero, and a packed payload of
        // width zero has no bytes for `tail_at` to read.
        let column = keys(&[42]);
        let map = KeyMap::build(&column).expect("build");
        resolves(&column, &map);
        assert_eq!(map.lookup(41).expect("lookup"), None);
        assert_eq!(map.lookup(43).expect("lookup"), None);
    }

    #[test]
    fn negative_keys_resolve_because_the_base_is_the_minimum_and_not_zero() {
        let column = keys(&[-9000, -3, -1, 0, 7]);
        let map = KeyMap::build(&column).expect("build");
        resolves(&column, &map);
        assert_eq!(map.lookup(-9001).expect("lookup"), None);
    }

    #[test]
    fn a_column_spanning_more_than_a_u64_of_values_is_refused_and_not_panicked_over() {
        // `max - min` on a HUGEINT column holding both ends of the type overflows an i128, so this
        // is where a build panics if the range arithmetic is done in the column's own type. It is
        // refused instead, and the caller records the relationship as not built.
        let column = keys(&[i128::MIN, 0, i128::MAX]);
        let error = KeyMap::build(&column).expect_err("refused");
        assert!(error.to_string().contains("spans more than a u64"), "{error}");
    }

    #[test]
    fn keys_at_the_far_end_of_the_integer_type_resolve_when_their_range_is_narrow() {
        // The other half of the same arithmetic: the values are extreme and the range is not, which
        // is a column a key map has to handle rather than refuse.
        let column = keys(&[i128::MIN, i128::MIN + 5, i128::MIN + 2]);
        let map = KeyMap::build(&column).expect("build");
        resolves(&column, &map);
        assert_eq!(map.lookup(i128::MAX).expect("lookup"), None);
        assert_eq!(map.lookup(0).expect("lookup"), None);
    }

    #[test]
    fn the_rank_index_agrees_with_counting_the_bits_by_hand() {
        // The rank structure is two levels and an eight word popcount, and an off by one in any of
        // the three resolves every key past the fault to the row before or after the right one. So
        // it is checked against the naive count over a bitmap wide enough to use every level: 4096
        // bits is one superblock exactly, so 20,000 forces five of them and the last one partial.
        let column = keys(&(0..10_000).map(|value| value * 2).collect::<Vec<i128>>());
        let map = KeyMap::build(&column).expect("build");
        assert_eq!(map.form(), Form::Dense);
        resolves(&column, &map);
    }

    #[test]
    fn a_string_key_arrives_as_dictionary_codes_and_never_as_text() {
        // Section 2.2's composition with the global dictionary. There is nothing string shaped in
        // this crate and that is the point: the codes of a file wide stable dictionary carry the
        // column's own order, so a sorted key map over a VARCHAR is this and the search is over
        // integers.
        let codes = keys(&[7, 1, 4, 9, 2]);
        let map = KeyMap::build(&codes).expect("build");
        resolves(&codes, &map);
    }

    #[test]
    fn the_form_tag_round_trips_and_an_unknown_one_is_refused() {
        for form in [Form::Identity, Form::Dense, Form::Sorted, Form::Permuted] {
            assert_eq!(Form::from_tag(form.tag()).expect("a known tag"), form);
        }
        assert!(Form::from_tag(4).is_err(), "an unfamiliar form is refused rather than guessed");
    }

    #[test]
    fn the_dense_form_costs_a_bitmap_and_about_an_eighth_again() {
        // The space claim in section 3.3, checked. A range of 80,000 bits is 10,000 bytes and the
        // index is a u16 per 512 bits plus a u32 per 4096, which is about 12.5 percent.
        let column = keys(&(0..10_000).map(|value| value * 8).collect::<Vec<i128>>());
        let map = KeyMap::build(&column).expect("build");
        assert_eq!(map.form(), Form::Dense);
        let bitmap = 80_000 / 8;
        let bytes = map.bytes();
        assert!(bytes > bitmap, "the map took {bytes} bytes and the bitmap alone is {bitmap}");
        assert!(
            bytes < bitmap * 5 / 4,
            "the map took {bytes} bytes, more than a quarter over the bitmap's {bitmap}"
        );
    }

    /// A column that counts how many times it was read, so a test can say what a build cost.
    struct Counted {
        column: Vec<Option<i128>>,
        scans: std::cell::Cell<usize>,
    }

    impl Keys for Counted {
        fn scan(&self, each: &mut dyn FnMut(Option<i128>) -> Result<()>) -> Result<()> {
            self.scans.set(self.scans.get() + 1);
            self.column.scan(each)
        }
    }

    #[test]
    fn a_build_from_a_scan_is_the_same_map_as_a_build_from_a_slice() {
        // The two builds have to agree on every column, because the streaming one is not a second
        // implementation, it is the same decision carried out against a source that is read twice.
        // If these ever disagree, a table's key map depends on which path built it.
        let columns: Vec<Vec<Option<i128>>> = vec![
            Vec::new(),
            keys(&[]),
            keys(&(1..=1000).collect::<Vec<i128>>()),
            keys(&(0..500).map(|value| value * 4).collect::<Vec<i128>>()),
            keys(&[100, 3, 40, 7, 9000]),
            keys(&[5, 5, 9]),
            vec![Some(10), None, Some(20), None, Some(30)],
            vec![None, None],
            shuffled(1_000, 4),
            keys(&[1, 3, 2, 1]),
        ];
        for column in &columns {
            let held = KeyMap::build(column).expect("build from a slice");
            let read = KeyMap::build_from(&column[..]).expect("build from a scan");
            assert_eq!(read.form(), held.form(), "{column:?}");
            assert_eq!(read.observed(), held.observed(), "{column:?}");
            assert_eq!(read.len(), held.len(), "{column:?}");
            assert_eq!(read.bytes(), held.bytes(), "{column:?}");
            // A column with a repeat in it has no one right row for its key, which is exactly why
            // section 2.3 refuses to build a link on one. So the round trip is checked where the
            // question has an answer.
            if read.observed().usable_as_parent() {
                resolves(column, &read);
            }
        }
    }

    #[test]
    fn the_identity_form_is_built_without_reading_the_column_twice() {
        // The reason `build_from` exists. Every TPC-H parent key takes the identity form, and the
        // identity form is two numbers, so a build of one has no business holding fifteen million
        // values or reading them a second time.
        let identity =
            Counted { column: keys(&(1..=1000).collect::<Vec<i128>>()), scans: 0.into() };
        assert_eq!(KeyMap::build_from(&identity).expect("build").form(), Form::Identity);
        assert_eq!(
            identity.scans.get(),
            1,
            "the identity form is the observation and nothing more"
        );

        // The other two forms have something to fill, so they read it again, and once is the number
        // that matters: a form that scanned per value would be a build nobody could afford.
        let dense = Counted {
            column: keys(&(0..500).map(|v| v * 4).collect::<Vec<i128>>()),
            scans: 0.into(),
        };
        assert_eq!(KeyMap::build_from(&dense).expect("build").form(), Form::Dense);
        assert_eq!(dense.scans.get(), 2);

        let sorted = Counted { column: keys(&[100, 3, 40, 7, 9000]), scans: 0.into() };
        assert_eq!(KeyMap::build_from(&sorted).expect("build").form(), Form::Sorted);
        assert_eq!(sorted.scans.get(), 2);

        // The permuted form reads it twice more, once to mark the bitmap and once to place the
        // rows, rather than holding it.
        let permuted = Counted { column: shuffled(1_000, 4), scans: 0.into() };
        assert_eq!(KeyMap::build_from(&permuted).expect("build").form(), Form::Permuted);
        assert_eq!(permuted.scans.get(), 3);
    }

    /// `count` keys a `step` apart from zero, stored in an order that is not theirs.
    ///
    /// The order is a multiplication by a prime modulo the count, so it is the same every run and
    /// visits every key once.
    fn shuffled(count: i128, step: i128) -> Vec<Option<i128>> {
        (0..count).map(|at| Some((at * 7_919 % count) * step)).collect()
    }

    #[test]
    fn dense_keys_stored_out_of_key_order_take_the_permuted_form() {
        // `orders` stored by date: the same keys, one in four of their range, in an order that has
        // nothing to do with them. The dense form cannot answer it and the sorted form costs its
        // keys and its permutation, which on SF1 was 8.25 MB and over the budget.
        let column = shuffled(10_000, 4);
        let map = KeyMap::build(&column).expect("build");
        assert_eq!(map.form(), Form::Permuted);
        assert!(map.observed().distinct);
        assert!(!map.observed().sorted);
        resolves(&column, &map);
        assert_eq!(map.lookup(1).expect("lookup"), None, "a key between two keys is not a key");
        assert_eq!(map.lookup(40_000).expect("lookup"), None, "past the end");
        assert_eq!(map.span(), Some((0, 39_997)), "the span is about the keys and not the rows");
        // The sorted form would hold a 16 bit key and a 14 bit rid a row. This holds a bit per value
        // of the range, the rank index over it, and the rid.
        let sorted = 10_000 * (16 + 14) / 8;
        assert!(map.bytes() * 10 < sorted * 7, "{} bytes against {sorted}", map.bytes());
    }

    #[test]
    fn a_repeat_that_is_not_adjacent_keeps_a_dense_column_out_of_every_form() {
        // The first scan only sees a repeat that sits next to itself. The bitmap sees every one,
        // because a key is marked twice only when two rows hold it.
        let column = keys(&[4, 1, 3, 2, 4]);
        let map = KeyMap::build(&column).expect("build");
        assert!(!map.observed().distinct);
        assert!(!map.observed().usable_as_parent());
        assert_eq!(map.lookup(4).expect("lookup"), None);
        let read = KeyMap::build_from(&column[..]).expect("build");
        assert_eq!(read.observed(), map.observed());
    }

    #[test]
    fn a_scan_that_fails_stops_the_build_rather_than_half_finishing_it() {
        struct Broken;
        impl Keys for Broken {
            fn scan(&self, _: &mut dyn FnMut(Option<i128>) -> Result<()>) -> Result<()> {
                Err(malformed("the column could not be read"))
            }
        }
        let error = KeyMap::build_from(&Broken).expect_err("a build over an unreadable column");
        assert!(error.to_string().contains("could not be read"), "{error}");
    }
}
