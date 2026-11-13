// Lifted from rudb crates/rudb-graph/src/span.rs at cd9f9676 (2026-10-05).
// Changes: errors of malformed bytes are XX001 `Error::corrupted`.

//! How far a child's dates sit from its parent's.
//!
//! spec/stats/07-graph-statistics.md section 7.9. For a pair of date columns, one in the child and
//! one in the parent, the smallest and the largest of `child - parent` over every child row that
//! found a parent. TPC-H ships a line one to 121 days after its order was placed, commits it 30 to
//! 90 days after, and receives it 2 to 151 days after, and a schema that records when things happen
//! to a row and to the row it belongs to has spans like these whether or not anybody wrote them
//! down as a constraint.
//!
//! What a span is for is carrying a restriction across the relationship. `l_shipdate > d` over a
//! join to `orders` says `o_orderdate > d - 121`, and on SF1 that is the difference between building
//! a hash table over 147,000 orders and building it over 15,000. The planner does that with it, see
//! `rudb_opt::span`, and a span nobody asks for costs 28 bytes.
//!
//! # Why every linked pair and not a sample
//!
//! A span is used to drop rows, so a span that is narrower than the truth drops a row the query
//! wanted. It is measured over every child row that found a parent or it is not written, which is
//! the same rule every other statistic that licenses a rewrite follows here.
//!
//! A null on one side decides which way a span may be used. A test carried from the child onto the
//! parent's column drops every parent without a date, so a linked child with a date and a parent
//! without one means the span cannot be carried that way. The same holds the other way round, and a
//! span records the two answers apart, since TPC-H's dates are never null and a schema whose child
//! dates sometimes are can still carry a child's test to its parent.

use rupg_common::{Error, Result};

/// The payload layout version. See the same constant in `link.rs` for why it is belt and braces.
const LAYOUT: u8 = 1;

/// Bytes one span costs on disk: two column numbers, two differences and the directions.
pub const BYTES: usize = 4 + 4 + 8 + 8 + 4;

/// The flag for a span a test on the child's column can be carried across onto the parent's.
const ONTO_PARENT: u32 = 1;

/// The flag for a span a test on the parent's column can be carried across onto the child's.
const ONTO_CHILD: u32 = 2;

/// One pair of date columns across a relationship and how far apart they ever are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// The child table's column.
    pub child: u32,
    /// The parent table's column.
    pub parent: u32,
    /// The smallest `child - parent` over the linked rows, in days.
    pub low: i64,
    /// The largest.
    pub high: i64,
    /// Whether every linked child row with a date has a parent with one, so a test on the child's
    /// column can be carried onto the parent's.
    pub onto_parent: bool,
    /// Whether every linked child row whose parent has a date has one of its own, so a test on the
    /// parent's column can be carried onto the child's.
    pub onto_child: bool,
}

/// Measures one span as the linked rows are walked, and says at the end whether it holds.
#[derive(Debug, Clone, Copy)]
pub struct Measure {
    child: u32,
    parent: u32,
    low: i64,
    high: i64,
    seen: bool,
    onto_parent: bool,
    onto_child: bool,
    broken: bool,
}

impl Measure {
    /// Nothing seen yet for this pair.
    #[must_use]
    pub const fn new(child: u32, parent: u32) -> Self {
        Self {
            child,
            parent,
            low: i64::MAX,
            high: i64::MIN,
            seen: false,
            onto_parent: true,
            onto_child: true,
            broken: false,
        }
    }

    /// One linked child row, with its date and its parent's, either of which may be missing.
    pub fn add(&mut self, child: Option<i64>, parent: Option<i64>) {
        match (child, parent) {
            (None, None) => {}
            (None, Some(_)) => self.onto_child = false,
            (Some(_), None) => self.onto_parent = false,
            (Some(child), Some(parent)) => match child.checked_sub(parent) {
                Some(gap) => {
                    self.low = self.low.min(gap);
                    self.high = self.high.max(gap);
                    self.seen = true;
                }
                None => self.broken = true,
            },
        }
    }

    /// The span, or `None` when no row had both dates or nulls rule out both directions.
    #[must_use]
    pub const fn finish(self) -> Option<Span> {
        if self.broken || !self.seen || !(self.onto_parent || self.onto_child) {
            return None;
        }
        Some(Span {
            child: self.child,
            parent: self.parent,
            low: self.low,
            high: self.high,
            onto_parent: self.onto_parent,
            onto_child: self.onto_child,
        })
    }
}

/// The payload for a set of spans: a layout byte, a count, then each span.
pub fn write(spans: &[Span], out: &mut Vec<u8>) {
    out.push(LAYOUT);
    out.extend_from_slice(&u32::try_from(spans.len()).unwrap_or(u32::MAX).to_le_bytes());
    for span in spans.iter().take(u32::MAX as usize) {
        out.extend_from_slice(&span.child.to_le_bytes());
        out.extend_from_slice(&span.parent.to_le_bytes());
        out.extend_from_slice(&span.low.to_le_bytes());
        out.extend_from_slice(&span.high.to_le_bytes());
        let flags = if span.onto_parent { ONTO_PARENT } else { 0 }
            | if span.onto_child { ONTO_CHILD } else { 0 };
        out.extend_from_slice(&flags.to_le_bytes());
    }
}

/// The spans a payload holds.
///
/// # Errors
///
/// If the layout byte is one this does not know, or the length is not what the count says, or a
/// span has its low above its high or a flag this does not know.
pub fn read(bytes: &[u8]) -> Result<Vec<Span>> {
    let [layout, count @ ..] = bytes else { return Err(malformed("an empty payload")) };
    if *layout != LAYOUT {
        return Err(malformed(format!("layout {layout}")));
    }
    let (count, rest) = count.split_at_checked(4).ok_or_else(|| malformed("no count"))?;
    let count = u32::from_le_bytes(count.try_into().map_err(|_| malformed("no count"))?) as usize;
    if rest.len() != count.saturating_mul(BYTES) {
        return Err(malformed(format!("{} bytes for {count} spans", rest.len())));
    }
    let word = |at: &[u8]| -> [u8; 8] { at.try_into().unwrap_or_default() };
    let half = |at: &[u8]| -> [u8; 4] { at.try_into().unwrap_or_default() };
    let mut spans = Vec::with_capacity(count);
    for one in rest.as_chunks::<BYTES>().0 {
        let flags = u32::from_le_bytes(half(&one[24..28]));
        if flags & !(ONTO_PARENT | ONTO_CHILD) != 0 {
            return Err(malformed(format!("flags {flags:#x}")));
        }
        let span = Span {
            child: u32::from_le_bytes(half(&one[0..4])),
            parent: u32::from_le_bytes(half(&one[4..8])),
            low: i64::from_le_bytes(word(&one[8..16])),
            high: i64::from_le_bytes(word(&one[16..24])),
            onto_parent: flags & ONTO_PARENT != 0,
            onto_child: flags & ONTO_CHILD != 0,
        };
        if span.low > span.high {
            return Err(malformed(format!("a span from {} to {}", span.low, span.high)));
        }
        spans.push(span);
    }
    Ok(spans)
}

fn malformed(message: impl Into<String>) -> Error {
    Error::corrupted(format!("invalid link span: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::{Measure, Span, read, write};

    #[test]
    fn a_span_is_the_smallest_and_largest_gap() {
        let mut measure = Measure::new(3, 1);
        for (child, parent) in [(10, 9), (130, 9), (50, 20)] {
            measure.add(Some(child), Some(parent));
        }
        assert_eq!(measure.finish(), Some(span(3, 1, 1, 121)));
    }

    /// A span with no nulls on either side, which can be carried both ways.
    fn span(child: u32, parent: u32, low: i64, high: i64) -> Span {
        Span { child, parent, low, high, onto_parent: true, onto_child: true }
    }

    #[test]
    fn a_null_on_one_side_rules_out_carrying_a_test_onto_that_side() {
        let mut child = Measure::new(0, 0);
        child.add(None, Some(5));
        child.add(Some(7), Some(5));
        child.add(None, None);
        let onto_parent = Span { onto_child: false, ..span(0, 0, 2, 2) };
        assert_eq!(child.finish(), Some(onto_parent));
        let mut parent = Measure::new(0, 0);
        parent.add(Some(7), Some(5));
        parent.add(Some(7), None);
        assert_eq!(parent.finish(), Some(Span { onto_parent: false, ..span(0, 0, 2, 2) }));
        let mut both = parent;
        both.add(None, Some(1));
        assert_eq!(both.finish(), None);
    }

    #[test]
    fn nothing_seen_is_no_span_rather_than_an_empty_one() {
        assert_eq!(Measure::new(0, 0).finish(), None);
    }

    #[test]
    fn what_is_written_is_what_is_read() {
        let spans = vec![span(10, 4, 1, 121), Span { onto_child: false, ..span(11, 4, -3, 90) }];
        let mut bytes = Vec::new();
        write(&spans, &mut bytes);
        assert_eq!(read(&bytes).expect("it reads"), spans);
        assert!(read(&bytes[..bytes.len() - 1]).is_err());
        let last = bytes.len() - 4;
        bytes[last] = 4;
        assert!(read(&bytes).is_err());
        let mut empty = Vec::new();
        write(&[], &mut empty);
        assert_eq!(read(&empty).expect("it reads"), Vec::new());
    }
}
