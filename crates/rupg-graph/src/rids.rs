// Lifted from rudb crates/rudb-graph/src/rids.rs at cd9f9676 (2026-10-05).
// Changes: errors of malformed bytes are XX001 `Error::corrupted`.

//! A set of row ids of one table, and pushing one through a link.
//!
//! spec/graph/04-in-memory.md section 4.3. This is the type a semi join reduction produces and
//! consumes: a predicate on a parent table leaves a set of parent rows, the set is pushed through
//! the forward link to become a set of child rows, and the child's scan reads only those. Section
//! 5.4 of the execution document is the argument for why that is worth doing and this module is the
//! part of it that has to be cheap.
//!
//! # Three forms, and why the choice is not the caller's
//!
//! Full is every row, which is a flag and nothing allocated. It exists because a reduction that
//! removed nothing has to cost nothing downstream, and without it that reduction costs a bitmap of
//! all ones and a test per row to learn nothing.
//!
//! Sparse is a sorted list of row ids, used below one member in [`SPARSE_RATIO`] rows. A test is a
//! binary search, which is fine because the consumer of a set that small walks it rather than
//! testing into it.
//!
//! Dense is one bit per row. On TPC-H SF100 `lineitem` that is 75 MB and `orders` is 18.75 MB, which
//! fits the last level cache of nothing, and the reason it is still the right form is that a scan
//! tests it in row id order, so the access is a stream rather than a scatter.
//!
//! Every constructor picks the form from the count, so the form is a function of the members and
//! the table size and nothing else. That is what makes two sets over the same rows with the same
//! members compare equal, and it means a caller never has to ask which one it got.
//!
//! # What is left out
//!
//! Section 4.3 gives the dense form a rank index. Nothing here asks a rank of one yet, and an index
//! nothing reads is an eighth more memory to build on every push, so it arrives with the first
//! caller that needs it.

use rupg_common::{Error, Result};

use crate::bits::BitVector;
use crate::link::Link;
use crate::rid::{NO_PARENT, PART_ROWS, Rid};

/// Below one member in this many rows, a set is held as a sorted list rather than a bitmap.
///
/// Section 4.3's number. At one in a thousand the list is eight bytes a member against a bitmap's
/// thousand bits, so the list is about a sixteenth of the size, and it stays smaller until one in
/// sixty four, so the threshold is on the side of the bitmap. That side is the one a scan wants.
pub const SPARSE_RATIO: u64 = 1000;

/// A push that stops early decides at the first part past one in this many child rows.
///
/// Section 5.4's third. Less and a set that removes rows only toward the end of a table clustered
/// by its parent is given up on while it is still about to pay. More and the push that removes
/// nothing costs most of what finishing it would have.
pub const STOP_AFTER: u64 = 3;

/// Which form a set is held in, for a plan output to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    /// Every row, with nothing allocated.
    Full,
    /// A sorted list of row ids.
    Sparse,
    /// One bit per row.
    Dense,
}

/// A set of row ids of one table of a known number of rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rids {
    rows: u64,
    body: Body,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Body {
    Full,
    Sparse(Vec<Rid>),
    Dense { words: Vec<u64>, members: u64 },
}

impl Rids {
    /// Every row of a table of `rows`.
    #[must_use]
    pub fn full(rows: u64) -> Self {
        if rows == 0 { Self::none(0) } else { Self { rows, body: Body::Full } }
    }

    /// No row of a table of `rows`.
    #[must_use]
    pub fn none(rows: u64) -> Self {
        Self { rows, body: Body::Sparse(Vec::new()) }
    }

    /// The set holding exactly `members`, which have to be strictly increasing and below `rows`.
    ///
    /// # Errors
    ///
    /// If a member is out of order, repeated or past the end. A set that silently dropped one of
    /// those would be a reduction that removed a row which joins.
    pub fn from_sorted(rows: u64, members: Vec<Rid>) -> Result<Self> {
        let mut previous = None;
        for &member in &members {
            if member >= rows || previous.is_some_and(|previous| member <= previous) {
                return Err(Error::internal(format!(
                    "row {member} is out of order or past the end of a table of {rows} rows"
                )));
            }
            previous = Some(member);
        }
        Ok(Self::settle_sparse(rows, members))
    }

    /// The set whose members are the set bits of `words`, least significant bit of word zero first.
    ///
    /// # Errors
    ///
    /// If `words` is not the number of words `rows` bits take, or a bit past `rows` is set.
    pub fn from_words(rows: u64, words: Vec<u64>) -> Result<Self> {
        if count(words.len()) != rows.div_ceil(64) {
            return Err(Error::internal(format!(
                "{} words is not a bitmap over {rows} rows",
                words.len()
            )));
        }
        let tail = rows % 64;
        if tail != 0 && words.last().is_some_and(|last| last >> tail != 0) {
            return Err(Error::internal("a bitmap has rows set past the end of its table"));
        }
        Ok(Self::settle_dense(rows, words))
    }

    /// Rows in the table this is a set over.
    #[must_use]
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// Rows in the set.
    #[must_use]
    pub fn len(&self) -> u64 {
        match &self.body {
            Body::Full => self.rows,
            Body::Sparse(members) => count(members.len()),
            Body::Dense { members, .. } => *members,
        }
    }

    /// Whether no row is in the set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether every row is in the set.
    #[must_use]
    pub fn is_full(&self) -> bool {
        matches!(self.body, Body::Full)
    }

    /// Which form the set is held in.
    #[must_use]
    pub fn form(&self) -> Form {
        match self.body {
            Body::Full => Form::Full,
            Body::Sparse(_) => Form::Sparse,
            Body::Dense { .. } => Form::Dense,
        }
    }

    /// Bytes the set holds on to.
    #[must_use]
    pub fn bytes(&self) -> usize {
        match &self.body {
            Body::Full => 0,
            Body::Sparse(members) => members.len() * size_of::<Rid>(),
            Body::Dense { words, .. } => words.len() * size_of::<u64>(),
        }
    }

    /// Whether `rid` is in the set.
    #[must_use]
    pub fn contains(&self, rid: Rid) -> bool {
        if rid >= self.rows {
            return false;
        }
        match &self.body {
            Body::Full => true,
            Body::Sparse(members) => members.binary_search(&rid).is_ok(),
            Body::Dense { words, .. } => bit(words, rid),
        }
    }

    /// Whether any member is between `low` and `high`, both included.
    ///
    /// The question a part skip asks: the link's zone map says the children of this part point at
    /// parents in that range, and a range holding no member is a part that cannot contribute.
    #[must_use]
    pub fn any_between(&self, low: Rid, high: Rid) -> bool {
        let high = high.min(self.rows.saturating_sub(1));
        if low > high || self.rows == 0 {
            return false;
        }
        match &self.body {
            Body::Full => true,
            Body::Sparse(members) => {
                let from = members.partition_point(|&member| member < low);
                members.get(from).is_some_and(|&member| member <= high)
            }
            Body::Dense { words, .. } => {
                let (first, last) = (index(low / 64), index(high / 64));
                (first..=last).any(|at| {
                    let mut word = words[at];
                    if at == first {
                        word &= u64::MAX << (low % 64);
                    }
                    if at == last {
                        word &= u64::MAX >> (63 - high % 64);
                    }
                    word != 0
                })
            }
        }
    }

    /// How many of the runs of `part` rows the table splits into hold a member.
    ///
    /// What a scan asks before reading a set's rows, since a part with none is skipped. A dense set
    /// answers a word at a time rather than a member at a time, which on q09 was 17M instructions
    /// for 320 thousand members.
    #[must_use]
    pub fn parts_touched(&self, part: u64) -> u64 {
        if part == 0 || self.rows == 0 {
            return 0;
        }
        match &self.body {
            Body::Full => self.rows.div_ceil(part),
            Body::Sparse(members) => {
                let (mut touched, mut last) = (0, u64::MAX);
                for &member in members {
                    if member / part != last {
                        last = member / part;
                        touched += 1;
                    }
                }
                touched
            }
            Body::Dense { .. } => {
                let parts = self.rows.div_ceil(part);
                count(
                    (0..parts)
                        .filter(|&at| self.any_between(at * part, at * part + part - 1))
                        .count(),
                )
            }
        }
    }

    /// The members, in increasing order.
    pub fn iter(&self) -> impl Iterator<Item = Rid> + '_ {
        let (full, sparse, dense) = match &self.body {
            Body::Full => (Some(0..self.rows), None, None),
            Body::Sparse(members) => (None, Some(members.iter().copied()), None),
            Body::Dense { words, .. } => (None, None, Some(ones(words))),
        };
        full.into_iter()
            .flatten()
            .chain(sparse.into_iter().flatten())
            .chain(dense.into_iter().flatten())
    }

    /// The rows in both sets.
    ///
    /// # Errors
    ///
    /// If the two are over tables of different sizes, which is two different tables.
    pub fn intersect(&self, other: &Self) -> Result<Self> {
        self.same_table(other)?;
        Ok(match (&self.body, &other.body) {
            (Body::Full, _) => other.clone(),
            (_, Body::Full) => self.clone(),
            (Body::Dense { words: left, .. }, Body::Dense { words: right, .. }) => {
                let words = left.iter().zip(right).map(|(left, right)| left & right).collect();
                Self::settle_dense(self.rows, words)
            }
            // A sparse side is small by definition, so the answer is the part of it the other side
            // holds, which is one test per member of the small one.
            (Body::Sparse(members), _) => Self::settle_sparse(
                self.rows,
                members.iter().copied().filter(|&member| other.contains(member)).collect(),
            ),
            (_, Body::Sparse(members)) => Self::settle_sparse(
                self.rows,
                members.iter().copied().filter(|&member| self.contains(member)).collect(),
            ),
        })
    }

    /// The rows in either set.
    ///
    /// # Errors
    ///
    /// If the two are over tables of different sizes.
    pub fn union(&self, other: &Self) -> Result<Self> {
        self.same_table(other)?;
        if self.is_full() || other.is_full() {
            return Ok(Self::full(self.rows));
        }
        let mut words = self.words();
        for member in other.iter() {
            words[index(member / 64)] |= 1 << (member % 64);
        }
        Ok(Self::settle_dense(self.rows, words))
    }

    /// Pushes a set of parent rows forward through `link`, to the child rows that point into it.
    ///
    /// One pass over the link in child order, which is section 4.3's first push. A part whose zone
    /// map says its children point only at parents outside the set is never decoded, which is
    /// section 5.5's part skip and on a child clustered by the parent is most of the table.
    ///
    /// # Errors
    ///
    /// If this set is not over the link's parent table.
    pub fn forward(&self, link: &Link) -> Result<Pushed> {
        self.push(link, false)
    }

    /// The same push, giving up once it is plain that the set removes nothing.
    ///
    /// Section 5.4's early stop. A push that has covered the first [`STOP_AFTER`]th of the child and
    /// kept every row of it stops there and hands back every row, so a reduction that was never
    /// going to remove anything costs a third of a push and then nothing, where finishing would
    /// cost the rest of the push and a bit test per row of the scan. The answer is then a superset
    /// of the children that point into the set, which is all a join that still matches every row
    /// needs, and [`Pushed::stopped`] says so.
    ///
    /// # Errors
    ///
    /// If this set is not over the link's parent table.
    pub fn forward_or_stop(&self, link: &Link) -> Result<Pushed> {
        self.push(link, true)
    }

    /// How many parts of the child a push of this set would decode, and how many there are.
    ///
    /// The zone test the push makes before it decodes a part, made for every part and nothing else,
    /// which is one range test per thousand child rows. A push saves the scan what it skips and
    /// costs a decode of the link and a bit test for every row of what it does not, so this is how
    /// a caller finds out which of the two it is before paying for either.
    ///
    /// # Errors
    ///
    /// If this set is not over the link's parent table.
    pub fn reach(&self, link: &Link) -> Result<(u64, u64)> {
        if self.rows != link.parents() {
            return Err(Error::internal(format!(
                "a set over {} rows reached through a link whose parent has {}",
                self.rows,
                link.parents()
            )));
        }
        let parts = link.children().div_ceil(count(PART_ROWS));
        let reached = (0..parts)
            .filter(|&part| match link.part_bounds(index(part)) {
                Some(Some((low, high))) => self.any_between(low, high),
                _ => false,
            })
            .count();
        Ok((count(reached), parts))
    }

    fn push(&self, link: &Link, stopping: bool) -> Result<Pushed> {
        if self.rows != link.parents() {
            return Err(Error::internal(format!(
                "a set over {} rows pushed through a link whose parent has {}",
                self.rows,
                link.parents()
            )));
        }
        let children = link.children();
        let parts = children.div_ceil(count(PART_ROWS));
        // Every child that has a parent is a member, which is every child when every child matched.
        if self.is_full() && link.linked() == children {
            return Ok(Pushed { rids: Self::full(children), parts, skipped: 0, stopped: false });
        }
        if let (Body::Sparse(members), Some(_)) = (&self.body, link.runs()) {
            return Self::push_members(members, link, children, parts);
        }
        if let Some(runs) = link.runs() {
            return Ok(self.push_runs(runs, children, parts, stopping));
        }
        let mut words = vec![0_u64; index(children.div_ceil(64))];
        let mut parents = vec![NO_PARENT; PART_ROWS];
        let mut skipped = 0_u64;
        // Asked once, at the first part boundary past the mark, because a push that has removed a
        // row by then has shown the set is worth finishing and asking again later would only give up
        // work already paid for.
        let mark = children.div_ceil(STOP_AFTER);
        let mut asked = !stopping;
        let mut kept = 0_u64;
        for part in 0..parts {
            let first = part * count(PART_ROWS);
            if !asked && first >= mark {
                asked = true;
                if kept == first {
                    return Ok(Pushed {
                        rids: Self::full(children),
                        parts,
                        skipped,
                        stopped: true,
                    });
                }
            }
            let reach = match link.part_bounds(index(part)) {
                Some(Some((low, high))) => self.any_between(low, high),
                _ => false,
            };
            if !reach {
                skipped += 1;
                continue;
            }
            let run = index((children - first).min(count(PART_ROWS)));
            link.forward_run(first, &mut parents[..run])?;
            for (at, &parent) in parents[..run].iter().enumerate() {
                if parent != NO_PARENT && self.contains(parent) {
                    let child = first + count(at);
                    words[index(child / 64)] |= 1 << (child % 64);
                    kept += 1;
                }
            }
        }
        Ok(Pushed { rids: Self::settle_dense(children, words), parts, skipped, stopped: false })
    }

    /// The push over a monotone link, a held parent at a time.
    ///
    /// The children of one parent are one run of ones in the link, so a parent the set holds keeps
    /// the whole run and one it does not keep none of it. The walk goes from one held parent to the
    /// next by counting the zeros between them a word at a time, and sets the range of bits for
    /// each run it lands on, so a parent the set does not hold costs its share of a word and not a
    /// step of its own. Stepping every parent and testing the set for each cost about fifty seven
    /// instructions a parent, which on TPC-H q21 was 85 million a push to keep a tenth of `orders`,
    /// see spec/perf/52-a-push-a-parent-at-a-time.md for where the step a parent came from.
    ///
    /// A part is counted as skipped when none of its rows is kept, which for a link in parent order
    /// is the part the zone map would have ruled out. The early stop is asked at the first run
    /// that starts past the mark rather than at a part boundary, which is the same question asked a
    /// few rows later at most.
    fn push_runs(&self, runs: &BitVector, children: u64, parts: u64, stopping: bool) -> Pushed {
        let mut words = vec![0_u64; index(children.div_ceil(64))];
        let bits = runs.words();
        let len = runs.len();
        let mark = children.div_ceil(STOP_AFTER);
        let mut asked = !stopping;
        // `at` is where the run of parent `parent` starts, which is past `parent` zeros, so the
        // children before it are `at - parent`.
        let (mut parent, mut at, mut kept) = (0_u64, 0_usize, 0_u64);
        for held in self.iter() {
            if held > parent {
                let Some(next) = past_zeros(bits, at, held - parent) else { break };
                at = next;
                parent = held;
            }
            if at > len {
                break;
            }
            let run = count(ones_from(bits, at, len));
            let child = count(at) - parent;
            if run > 0 {
                if !asked && child >= mark {
                    asked = true;
                    if kept == child {
                        return Pushed {
                            rids: Self::full(children),
                            parts,
                            skipped: 0,
                            stopped: true,
                        };
                    }
                }
                set_range(&mut words, child, child + run);
                kept += run;
            }
            // The zero after the run, which moves on to the next parent.
            at += index(run) + 1;
            parent += 1;
        }
        let per_part = PART_ROWS / 64;
        let skipped =
            count(words.chunks(per_part).filter(|part| part.iter().all(|&word| word == 0)).count());
        Pushed { rids: Self::settle_dense(children, words), parts, skipped, stopped: false }
    }

    /// The push of a sparse set over a monotone link, a member at a time.
    ///
    /// A member's children are one run that [`Link::backward`] finds with two selects, so this costs
    /// what the set holds and not what the parent holds. On TPC-H q18 the set is the 57 orders over
    /// 300 in quantity, and [`Self::push_runs`] stepped over all 1.5 million orders to find their
    /// 399 lines. The early stop is not asked, since a sparse set holds at most one parent in
    /// [`SPARSE_RATIO`] and the answer is exact either way.
    fn push_members(members: &[Rid], link: &Link, children: u64, parts: u64) -> Result<Pushed> {
        let mut kept = Vec::new();
        for &member in members {
            let children = link.backward(member).ok_or_else(|| {
                Error::internal(format!("parent {member} has no run in a monotone link"))
            })?;
            kept.extend(children);
        }
        let part = count(PART_ROWS);
        let reached = count(kept.chunk_by(|a, b| a / part == b / part).count());
        Ok(Pushed {
            rids: Self::settle_sparse(children, kept),
            parts,
            skipped: parts.saturating_sub(reached),
            stopped: false,
        })
    }

    /// The members from `first` for `len` rows, as offsets from `first`, in order.
    ///
    /// What a scan keeps of a part the set was pushed into. A sparse set answers from the members in
    /// the range, which is one search and then the members, where testing each row of the part was
    /// a search a row.
    #[must_use]
    pub fn offsets_in(&self, first: Rid, len: usize) -> Vec<u32> {
        let end = first.saturating_add(count(len)).min(self.rows);
        let offset = |rid: Rid| u32::try_from(rid - first).unwrap_or(u32::MAX);
        match &self.body {
            Body::Full => (first..end).map(offset).collect(),
            Body::Sparse(members) => {
                let from = members.partition_point(|&member| member < first);
                members[from..]
                    .iter()
                    .take_while(|&&member| member < end)
                    .map(|&m| offset(m))
                    .collect()
            }
            Body::Dense { words, .. } => {
                // Counted first, because pushing onto an empty vector grew it several times a part,
                // and a reduced scan of `cast_info` in JOB asks this of four thousand parts.
                let mut out = Vec::with_capacity(self.count_in(first, len));
                let mut at = first;
                while at < end {
                    let word = words.get(index(at / 64)).copied().unwrap_or(0) >> (at % 64);
                    let span = (64 - at % 64).min(end - at);
                    let mut bits = if span == 64 { word } else { word & ((1 << span) - 1) };
                    while bits != 0 {
                        out.push(offset(at + u64::from(bits.trailing_zeros())));
                        bits &= bits - 1;
                    }
                    at += span;
                }
                out
            }
        }
    }

    /// How many members there are from `first` for `len` rows, which is the length
    /// [`Self::offsets_in`] answers with, without writing them down.
    #[must_use]
    pub fn count_in(&self, first: Rid, len: usize) -> usize {
        let end = first.saturating_add(count(len)).min(self.rows);
        if first >= end {
            return 0;
        }
        match &self.body {
            Body::Full => index(end - first),
            Body::Sparse(members) => {
                members.partition_point(|&member| member < end)
                    - members.partition_point(|&member| member < first)
            }
            Body::Dense { words, .. } => {
                let (low, high) = (first / 64, (end - 1) / 64);
                let mut held = 0;
                for at in low..=high {
                    let mut word = words.get(index(at)).copied().unwrap_or(0);
                    if at == low {
                        word &= u64::MAX << (first % 64);
                    }
                    if at == high {
                        word &= u64::MAX >> (63 - (end - 1) % 64);
                    }
                    held += word.count_ones() as usize;
                }
                held
            }
        }
    }

    /// Pushes a set of child rows backward through `link`, to the parents they point at.
    ///
    /// The second push of section 4.3, a pass over the link setting a bit per surviving child. It
    /// reads the link in child order just as the forward push does, so it needs no backward
    /// structure, and a child without a parent contributes nothing.
    ///
    /// # Errors
    ///
    /// If this set is not over the link's child table.
    pub fn backward(&self, link: &Link) -> Result<Self> {
        if self.rows != link.children() {
            return Err(Error::internal(format!(
                "a set over {} rows pushed back through a link whose child has {}",
                self.rows,
                link.children()
            )));
        }
        let mut words = vec![0_u64; index(link.parents().div_ceil(64))];
        let mut parents = vec![NO_PARENT; PART_ROWS];
        let children = link.children();
        for part in 0..children.div_ceil(count(PART_ROWS)) {
            let first = part * count(PART_ROWS);
            let last = (first + count(PART_ROWS)).min(children) - 1;
            if !self.any_between(first, last) {
                continue;
            }
            let run = index(last - first + 1);
            link.forward_run(first, &mut parents[..run])?;
            for (at, &parent) in parents[..run].iter().enumerate() {
                if parent != NO_PARENT && self.contains(first + count(at)) {
                    words[index(parent / 64)] |= 1 << (parent % 64);
                }
            }
        }
        Ok(Self::settle_dense(link.parents(), words))
    }

    fn same_table(&self, other: &Self) -> Result<()> {
        if self.rows == other.rows {
            Ok(())
        } else {
            Err(Error::internal(format!(
                "a set over {} rows combined with one over {}",
                self.rows, other.rows
            )))
        }
    }

    /// The set as a bitmap, whatever form it is held in.
    fn words(&self) -> Vec<u64> {
        let mut words = vec![0_u64; index(self.rows.div_ceil(64))];
        match &self.body {
            Body::Dense { words: held, .. } => words.copy_from_slice(held),
            _ => {
                for member in self.iter() {
                    words[index(member / 64)] |= 1 << (member % 64);
                }
            }
        }
        words
    }

    /// The form a bitmap's members call for.
    fn settle_dense(rows: u64, words: Vec<u64>) -> Self {
        let members = words.iter().map(|word| u64::from(word.count_ones())).sum::<u64>();
        match shape(rows, members) {
            Form::Full => Self::full(rows),
            Form::Sparse => Self { rows, body: Body::Sparse(ones(&words).collect()) },
            Form::Dense => Self { rows, body: Body::Dense { words, members } },
        }
    }

    /// The form a sorted list's members call for.
    fn settle_sparse(rows: u64, members: Vec<Rid>) -> Self {
        match shape(rows, count(members.len())) {
            Form::Full => Self::full(rows),
            Form::Sparse => Self { rows, body: Body::Sparse(members) },
            Form::Dense => {
                let mut words = vec![0_u64; index(rows.div_ceil(64))];
                for member in &members {
                    words[index(member / 64)] |= 1 << (member % 64);
                }
                Self { rows, body: Body::Dense { words, members: count(members.len()) } }
            }
        }
    }
}

/// What pushing a set through a link produced, and how much of the link it had to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pushed {
    /// The child rows that point into the set.
    pub rids: Rids,
    /// Parts of the child table there are.
    pub parts: u64,
    /// Parts the zone map ruled out without their link being decoded.
    pub skipped: u64,
    /// Whether the push gave up early, so that `rids` is every row rather than exactly the ones
    /// that point into the set. Only [`Rids::forward_or_stop`] does.
    pub stopped: bool,
}

/// Which form `members` rows out of `rows` belong in.
fn shape(rows: u64, members: u64) -> Form {
    if rows > 0 && members == rows {
        Form::Full
    } else if members == 0 || members.saturating_mul(SPARSE_RATIO) < rows {
        Form::Sparse
    } else {
        Form::Dense
    }
}

/// Whether bit `at` of a bitmap is set.
fn bit(words: &[u64], at: u64) -> bool {
    words.get(index(at / 64)).is_some_and(|word| word >> (at % 64) & 1 == 1)
}

/// The bit just past the `n`th zero from bit `at` on, for `n` of one or more, or `None` when the
/// words run out first.
///
/// A word at a time: the zeros left in the word are counted, and either they are too few and the
/// walk moves on, or the one wanted is among them and is found with one select in the word.
pub(crate) fn past_zeros(bits: &[u64], mut at: usize, n: u64) -> Option<usize> {
    let mut left = n - 1;
    loop {
        let shift = at % 64;
        let zeros = !*bits.get(at / 64)? >> shift;
        let found = u64::from(zeros.count_ones());
        if found > left {
            #[expect(clippy::cast_possible_truncation, reason = "under the zeros in one word")]
            let within = crate::bits::nth_set(zeros, left as u32) as usize;
            return Some(at + within + 1);
        }
        left -= found;
        at += 64 - shift;
    }
}

/// How many one bits in a row start at bit `at`, stopping at `len`.
pub(crate) fn ones_from(bits: &[u64], at: usize, len: usize) -> usize {
    let mut end = at;
    while end < len {
        let shift = end % 64;
        // The shift brings in zeros at the top, which the negation turns into ones, so the count
        // stops at the end of the word at the latest.
        let word = bits.get(end / 64).copied().unwrap_or(0) >> shift;
        let found = (!word).trailing_zeros() as usize;
        if found < 64 - shift {
            return (end + found).min(len) - at;
        }
        end += 64 - shift;
    }
    len - at
}

/// Sets the bits from `from` up to but not including `to`.
fn set_range(words: &mut [u64], from: u64, to: u64) {
    let (mut at, to) = (index(from), index(to));
    while at < to {
        let shift = at % 64;
        let take = (64 - shift).min(to - at);
        let mask = if take == 64 { u64::MAX } else { ((1_u64 << take) - 1) << shift };
        words[at / 64] |= mask;
        at += take;
    }
}

/// The set bits of a bitmap, in order.
fn ones(words: &[u64]) -> impl Iterator<Item = Rid> + '_ {
    words.iter().enumerate().flat_map(|(at, &word)| {
        let base = count(at) * 64;
        let mut rest = word;
        std::iter::from_fn(move || {
            if rest == 0 {
                return None;
            }
            let low = u64::from(rest.trailing_zeros());
            rest &= rest - 1;
            Some(base + low)
        })
    })
}

/// A count in the `u64` every interface here uses.
fn count(rows: usize) -> u64 {
    u64::try_from(rows).unwrap_or(u64::MAX)
}

/// A row count as an index. Every set here fits in memory, so one that does not is a bug upstream.
fn index(rows: u64) -> usize {
    usize::try_from(rows).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::{Form, Rids, SPARSE_RATIO, STOP_AFTER};
    use crate::link::Link;
    use crate::rid::{NO_PARENT, PART_ROWS, Rid};

    /// The members of a set, the slow way, for comparing against.
    fn members(rids: &Rids) -> Vec<Rid> {
        (0..rids.rows()).filter(|&rid| rids.contains(rid)).collect()
    }

    #[test]
    fn the_offsets_of_a_bitmap_are_its_members_in_the_range() {
        let rows = 5000;
        let members: Vec<Rid> = (0..rows).filter(|rid| rid % 3 == 0 || rid % 64 == 63).collect();
        let set = Rids::from_sorted(rows, members.clone()).expect("sorted");
        assert_eq!(set.form(), Form::Dense);
        for (first, len) in [(0, 64), (1, 63), (63, 2), (100, 1000), (4990, 64), (0, 5000), (7, 0)]
        {
            let slow = members
                .iter()
                .filter(|&&rid| rid >= first && rid < first + len)
                .map(|&rid| u32::try_from(rid - first).expect("small"))
                .collect::<Vec<_>>();
            let len = usize::try_from(len).expect("small");
            assert_eq!(set.offsets_in(first, len), slow, "from {first} for {len}");
            assert_eq!(set.count_in(first, len), slow.len(), "counted from {first} for {len}");
        }
    }

    #[test]
    fn the_parts_touched_are_the_parts_a_member_falls_in() {
        let rows = 50_000;
        let dense: Vec<Rid> = (0..rows).filter(|rid| rid % 3 == 0 && rid % 7000 < 900).collect();
        let sparse: Vec<Rid> = vec![5, 2047, 2048, 30_000, 49_999];
        for members in [dense, sparse, (0..rows).collect(), Vec::new()] {
            let set = Rids::from_sorted(rows, members.clone()).expect("sorted");
            for part in [1, 63, 64, 100, 2048, 70_000] {
                let mut slow: Vec<Rid> = members.iter().map(|rid| rid / part).collect();
                slow.dedup();
                assert_eq!(
                    set.parts_touched(part),
                    slow.len() as u64,
                    "{:?} by {part}",
                    set.form()
                );
            }
        }
    }

    #[test]
    fn the_form_follows_the_count_and_not_the_constructor() {
        let rows = 10 * SPARSE_RATIO;
        assert_eq!(Rids::from_sorted(rows, vec![1, 2, 3]).expect("sorted").form(), Form::Sparse);
        let many: Vec<Rid> = (0..rows).step_by(2).collect();
        assert_eq!(Rids::from_sorted(rows, many).expect("sorted").form(), Form::Dense);
        let every: Vec<Rid> = (0..rows).collect();
        assert_eq!(Rids::from_sorted(rows, every).expect("sorted").form(), Form::Full);
        let mut words = vec![0_u64; usize::try_from(rows.div_ceil(64)).expect("small")];
        words[0] = 1;
        assert_eq!(Rids::from_words(rows, words).expect("bitmap").form(), Form::Sparse);
    }

    /// The reason the form is chosen by the count: the same members over the same table are the
    /// same set, however they were built.
    #[test]
    fn the_same_members_are_the_same_set_whichever_way_they_came_in() {
        let rows = 5000;
        let members: Vec<Rid> = (0..rows).filter(|rid| rid % 3 == 0).collect();
        let mut words = vec![0_u64; usize::try_from(rows.div_ceil(64)).expect("small")];
        for member in &members {
            words[usize::try_from(member / 64).expect("small")] |= 1 << (member % 64);
        }
        let listed = Rids::from_sorted(rows, members).expect("sorted");
        let mapped = Rids::from_words(rows, words).expect("bitmap");
        assert_eq!(listed, mapped);
    }

    #[test]
    fn a_member_out_of_order_or_past_the_end_is_refused() {
        assert!(Rids::from_sorted(10, vec![3, 2]).is_err());
        assert!(Rids::from_sorted(10, vec![3, 3]).is_err());
        assert!(Rids::from_sorted(10, vec![10]).is_err());
        assert!(Rids::from_words(10, vec![1 << 10]).is_err(), "a bit past the end");
        assert!(Rids::from_words(10, vec![0, 0]).is_err(), "a word too many");
    }

    #[test]
    fn a_full_set_holds_nothing_and_answers_everything() {
        let full = Rids::full(1_000_000);
        assert_eq!(full.bytes(), 0);
        assert_eq!(full.len(), 1_000_000);
        assert!(full.contains(999_999));
        assert!(!full.contains(1_000_000));
    }

    /// A part is reached when some child in it points at a parent in the set, which is the part a
    /// push would decode, and one whose children all point elsewhere is not.
    #[test]
    fn reach_counts_the_parts_a_push_would_decode() {
        let parents = 100;
        let parents_of: Vec<Rid> = (0..count(PART_ROWS) * 4)
            .map(|child| child * parents / (count(PART_ROWS) * 4))
            .collect();
        let link = Link::build(&parents_of, parents).expect("every parent exists");
        let one = Rids::from_sorted(parents, vec![30]).expect("a parent that exists");
        assert_eq!(one.reach(&link).expect("the same parent"), (1, 4));
        let two = Rids::from_sorted(parents, vec![10, 90]).expect("parents that exist");
        assert_eq!(two.reach(&link).expect("the same parent"), (2, 4));
        assert_eq!(Rids::full(parents).reach(&link).expect("the same parent"), (4, 4));
        assert!(Rids::full(parents + 1).reach(&link).is_err(), "a set over another table");
    }

    #[test]
    fn any_between_looks_only_inside_the_range_in_every_form() {
        let rows = 4096;
        for members in [vec![700], (0..rows).filter(|rid| rid % 2 == 0 && *rid != 700).collect()] {
            let rids = Rids::from_sorted(rows, members.clone()).expect("sorted");
            for (low, high) in [(0, 63), (64, 699), (699, 701), (700, 700), (1000, 5000)] {
                let expected = members.iter().any(|&member| (low..=high).contains(&member));
                assert_eq!(
                    rids.any_between(low, high),
                    expected,
                    "{low}..={high} over {:?}",
                    rids.form()
                );
            }
        }
        assert!(Rids::full(10).any_between(3, 3));
        assert!(!Rids::full(10).any_between(10, 20), "past the end is outside the table");
    }

    #[test]
    fn intersect_and_union_agree_with_the_slow_answer_across_forms() {
        let rows = 20_000;
        let sets = [
            Rids::none(rows),
            Rids::from_sorted(rows, vec![5, 700, 19_999]).expect("sorted"),
            Rids::from_sorted(rows, (0..rows).filter(|rid| rid % 3 == 0).collect())
                .expect("sorted"),
            Rids::from_sorted(rows, (0..rows).filter(|rid| rid % 5 == 0).collect())
                .expect("sorted"),
            Rids::full(rows),
        ];
        for left in &sets {
            for right in &sets {
                let both = left.intersect(right).expect("same table");
                let either = left.union(right).expect("same table");
                let (left_members, right_members) = (members(left), members(right));
                let expected_both: Vec<Rid> = left_members
                    .iter()
                    .copied()
                    .filter(|rid| right_members.contains(rid))
                    .collect();
                let mut expected_either = left_members.clone();
                expected_either.extend(right_members.iter().copied());
                expected_either.sort_unstable();
                expected_either.dedup();
                assert_eq!(members(&both), expected_both);
                assert_eq!(members(&either), expected_either);
                assert_eq!(both.iter().collect::<Vec<_>>(), expected_both, "iteration is in order");
            }
        }
        assert!(Rids::full(3).intersect(&Rids::full(4)).is_err(), "two different tables");
    }

    /// A child of `children` rows whose parents are `parent_of(child)`, over `parents` parents.
    fn link(children: u64, parents: u64, parent_of: impl Fn(u64) -> Rid) -> Link {
        let of: Vec<Rid> = (0..children).map(parent_of).collect();
        Link::build(&of, parents).expect("a link")
    }

    /// The forward push against the definition, over both forms of link, including a part of
    /// children that point at no parent at all.
    #[test]
    fn a_forward_push_finds_exactly_the_children_that_point_into_the_set() {
        let parents = 3000;
        let children = 10 * count(PART_ROWS) + 17;
        let clustered = link(children, parents, |child| child * parents / children);
        let scattered = link(children, parents, |child| {
            if child / count(PART_ROWS) == 4 { NO_PARENT } else { (child * 7919) % parents }
        });
        for link in [&clustered, &scattered] {
            for set in [
                Rids::none(parents),
                Rids::from_sorted(parents, vec![0, 1500, 2999]).expect("sorted"),
                Rids::from_sorted(parents, (0..parents).filter(|p| p % 4 == 1).collect())
                    .expect("sorted"),
                Rids::full(parents),
            ] {
                let pushed = set.forward(link).expect("the same table");
                let expected: Vec<Rid> = (0..children)
                    .filter(|&child| link.forward(child).is_some_and(|parent| set.contains(parent)))
                    .collect();
                assert_eq!(
                    members(&pushed.rids),
                    expected,
                    "{:?} through {:?}",
                    set.form(),
                    link.form()
                );
            }
        }
    }

    /// The monotone push a parent at a time against the definition a child at a time, on a link
    /// where some parents have no children and some have runs that cross words and parts.
    #[test]
    fn a_push_a_parent_at_a_time_keeps_exactly_the_children_of_the_parents_held() {
        let parents: u64 = 2000;
        // Parent p has p % 7 children, and every hundredth one has three hundred, so runs are
        // empty, short and longer than a word.
        let sizes: Vec<u64> =
            (0..parents).map(|p| if p % 100 == 42 { 300 } else { p % 7 }).collect();
        let of: Vec<Rid> =
            (0..parents).flat_map(|p| std::iter::repeat_n(p, index(sizes[index(p)]))).collect();
        let children = count(of.len());
        let link = Link::build(&of, parents).expect("a link");
        assert_eq!(link.form(), crate::link::Form::Monotone);
        for set in [
            Rids::none(parents),
            Rids::from_sorted(parents, vec![0, 42, 1999]).expect("sorted"),
            Rids::from_sorted(parents, (0..parents).filter(|p| p % 3 != 0).collect())
                .expect("sorted"),
            Rids::from_sorted(parents, (0..parents).filter(|p| p % 5 == 2).collect())
                .expect("sorted"),
            // Held parents far apart, so the walk crosses words of zeros and runs between them.
            Rids::from_sorted(parents, (0..parents).filter(|p| p % 97 == 42).collect())
                .expect("sorted"),
            Rids::from_sorted(parents, (1990..parents).collect()).expect("sorted"),
        ] {
            let pushed = set.forward(&link).expect("the same table");
            let expected: Vec<Rid> =
                (0..children).filter(|&child| set.contains(of[index(child)])).collect();
            assert_eq!(members(&pushed.rids), expected, "{:?}", set.form());
            let parts = children.div_ceil(count(PART_ROWS));
            let untouched = (0..parts)
                .filter(|part| !expected.iter().any(|child| child / count(PART_ROWS) == *part))
                .count();
            assert_eq!(pushed.skipped, count(untouched), "{:?}", set.form());
        }
    }

    /// A sparse set over a monotone link is pushed a member at a time. The answer and the parts it
    /// skips are the same as the definition's, including a member with no children, the last parent
    /// and a member whose run crosses a part.
    #[test]
    fn a_sparse_push_a_member_at_a_time_keeps_exactly_the_children_of_its_members() {
        let parents: u64 = 20 * SPARSE_RATIO;
        let sizes: Vec<u64> =
            (0..parents).map(|p| if p % 1000 == 42 { 3000 } else { p % 5 }).collect();
        let of: Vec<Rid> =
            (0..parents).flat_map(|p| std::iter::repeat_n(p, index(sizes[index(p)]))).collect();
        let children = count(of.len());
        let link = Link::build(&of, parents).expect("a link");
        assert_eq!(link.form(), crate::link::Form::Monotone);
        let set =
            Rids::from_sorted(parents, vec![0, 5, 42, 7042, 7043, parents - 1]).expect("sorted");
        assert_eq!(set.form(), Form::Sparse);
        let pushed = set.forward_or_stop(&link).expect("the same table");
        let expected: Vec<Rid> =
            (0..children).filter(|&child| set.contains(of[index(child)])).collect();
        assert_eq!(members(&pushed.rids), expected);
        let parts = children.div_ceil(count(PART_ROWS));
        let untouched = (0..parts)
            .filter(|part| !expected.iter().any(|child| child / count(PART_ROWS) == *part))
            .count();
        assert_eq!(
            (pushed.parts, pushed.skipped, pushed.stopped),
            (parts, count(untouched), false)
        );
    }

    /// The offsets a scan keeps out of a part are the members in it, for each of the three forms.
    #[test]
    fn the_offsets_in_a_range_are_its_members_counted_from_its_start() {
        let rows = 10 * SPARSE_RATIO;
        let sparse = Rids::from_sorted(rows, vec![3, 100, 101, 9999]).expect("sorted");
        let dense = Rids::from_sorted(rows, (0..rows).step_by(3).collect()).expect("sorted");
        for set in [sparse, dense, Rids::full(rows), Rids::none(rows)] {
            for (first, len) in [(0, 200), (100, 2), (9990, 50), (rows, 10)] {
                let expected: Vec<u32> =
                    (0..len).filter(|&at| set.contains(first + u64::from(at))).collect();
                assert_eq!(
                    set.offsets_in(first, index(u64::from(len))),
                    expected,
                    "{:?}",
                    set.form()
                );
                assert_eq!(set.count_in(first, index(u64::from(len))), expected.len());
            }
        }
    }

    fn index(rows: u64) -> usize {
        usize::try_from(rows).expect("small")
    }

    fn count(rows: usize) -> u64 {
        u64::try_from(rows).expect("small")
    }

    /// A set that holds every parent a child points at stops at the first part past the third and
    /// hands back every row, and a set that removes one row before the third finishes and is exact.
    #[test]
    fn a_push_that_removes_nothing_by_the_third_stops_and_one_that_removes_something_finishes() {
        let parents = 3000;
        let children = 4 * STOP_AFTER * count(PART_ROWS);
        let clustered = link(children, parents, |child| child * parents / children);
        let every = Rids::full(parents);
        let all_but_last: Vec<Rid> = (0..parents - 1).collect();
        let most = Rids::from_sorted(parents, all_but_last).expect("sorted");
        let stopped = most.forward_or_stop(&clustered).expect("the same table");
        assert!(stopped.stopped, "nothing was removed in the first third");
        assert!(stopped.rids.is_full(), "a stopped push keeps every row");
        assert_eq!(stopped.parts, 4 * STOP_AFTER);
        // The full set never reaches the loop, since it cannot remove anything to begin with.
        assert!(!every.forward_or_stop(&clustered).expect("the same table").stopped);

        let all_but_first: Vec<Rid> = (1..parents).collect();
        let early = Rids::from_sorted(parents, all_but_first).expect("sorted");
        let finished = early.forward_or_stop(&clustered).expect("the same table");
        assert!(!finished.stopped, "the first parent's children were removed before the third");
        assert_eq!(finished, early.forward(&clustered).expect("the same table"));
        assert_eq!(
            finished.rids.len(),
            children - count((0..children).filter(|child| child * parents / children == 0).count())
        );

        // A child with no parent is a row removed, the same as a child whose parent is not held.
        let orphans =
            link(children, parents, |child| if child == 5 { NO_PARENT } else { child % parents });
        assert!(!every.forward_or_stop(&orphans).expect("the same table").stopped);
    }

    /// Section 5.5's claim, on the shape it is made about: a child clustered by its parent, and a
    /// set of parents that is one contiguous stretch of them, reads only the parts over that stretch.
    #[test]
    fn a_clustered_child_skips_every_part_that_points_outside_the_set() {
        let parents = 1000;
        let children = 100 * count(PART_ROWS);
        let link = link(children, parents, |child| child * parents / children);
        let set = Rids::from_sorted(parents, (100..200).collect()).expect("sorted");
        let pushed = set.forward(&link).expect("the same table");
        assert_eq!(pushed.parts, 100);
        // A tenth of the parents is a tenth of the parts, give or take the two at the edges.
        assert!(pushed.skipped >= 88, "only {} of 100 parts were skipped", pushed.skipped);
        assert_eq!(pushed.rids.len(), children / 10);
    }

    #[test]
    fn nothing_in_the_set_skips_every_part_and_everything_skips_the_pass() {
        let link = link(5000, 100, |child| child % 100);
        let pushed = Rids::none(100).forward(&link).expect("the same table");
        assert_eq!((pushed.skipped, pushed.rids.len()), (pushed.parts, 0));
        let pushed = Rids::full(100).forward(&link).expect("the same table");
        assert!(pushed.rids.is_full(), "every child matched, so every child is in");
        assert_eq!(pushed.skipped, 0);
    }

    #[test]
    fn a_backward_push_finds_exactly_the_parents_the_set_points_at() {
        let parents = 500;
        let children = 7 * count(PART_ROWS) + 3;
        let clustered = link(children, parents, |child| child * parents / children);
        let scattered = link(children, parents, |child| {
            if child % 11 == 0 { NO_PARENT } else { (child * 31) % parents }
        });
        for link in [&clustered, &scattered] {
            let set = Rids::from_sorted(children, (0..children).filter(|c| c % 97 == 3).collect())
                .expect("sorted");
            let pushed = set.backward(link).expect("the same table");
            let mut expected: Vec<Rid> =
                set.iter().filter_map(|child| link.forward(child)).collect();
            expected.sort_unstable();
            expected.dedup();
            assert_eq!(members(&pushed), expected, "through {:?}", link.form());
        }
    }

    #[test]
    fn a_set_over_the_wrong_table_is_refused_rather_than_pushed() {
        let link = link(100, 10, |child| child % 10);
        assert!(Rids::full(11).forward(&link).is_err());
        assert!(Rids::full(10).backward(&link).is_err());
    }
}
