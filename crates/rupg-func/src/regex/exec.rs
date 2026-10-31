//! The matcher: the tree of the parser compiled to a program of a backtracking machine.
//!
//! The program of a pattern tells if the pattern matches, and where the leftmost match starts. The program of a node of the tree of subexpressions tells where a match of the node can end, which the dissection of a match needs.
//!
//! A program with no back reference keeps a set of the pairs of instruction and position that it has tried, so each pair is tried once and the search takes time in proportion to the size of the program and the length of the string. A program with back references cannot do this, because the result then depends on the groups, and it stops an empty loop with a mark of the position at the start of each pass.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use super::parse::{Assert, DUPINF, Look, Node, Set, to_lower};
use super::{Code, flags};

/// The largest number of instructions of a program, after the copies of the counted repeats.
const MAX_INSTS: usize = 1 << 20;

/// The largest set of tried pairs, in bits.
const MAX_VISITED: usize = 1 << 26;

/// The most states that the DFA of a program keeps. It drops all its states when it has more.
const MAX_STATES: usize = 4096;

/// The constraints, in the order of their bits in the mask of the constraints that hold at a position.
const ASSERTS: [Assert; 8] = [
    Assert::Bol,
    Assert::Eol,
    Assert::Bos,
    Assert::Eos,
    Assert::WordStart,
    Assert::WordEnd,
    Assert::Boundary,
    Assert::NotBoundary,
];

/// A fast hash of the keys of the DFA, which the DFA makes and reads only for itself.
#[derive(Clone, Copy, Default)]
struct Fx(u64);

impl Fx {
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

impl Hasher for Fx {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for word in words {
            self.add(u64::from_le_bytes(*word));
        }
        for &byte in rest {
            self.add(u64::from(byte));
        }
    }

    fn write_u32(&mut self, word: u32) {
        self.add(u64::from(word));
    }

    fn write_u64(&mut self, word: u64) {
        self.add(word);
    }

    fn write_usize(&mut self, word: usize) {
        self.add(word as u64);
    }
}

type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<Fx>>;

/// The DFA of the main program, which [`Program::ends`] builds as it reads strings. A state is a set of instructions `Set` that the threads wait at. The closure of a state follows the instructions that read no character, with the mask of the constraints that hold at the position, and the step of a closed state reads one character.
#[derive(Debug, Default)]
struct Dfa {
    ids: FxMap<Box<[usize]>, u32>,
    states: Vec<Box<[usize]>>,
    /// For each state, the closed states for the masks that the DFA met: the mask, the closed state and true when the closure has `Match`.
    closed: Vec<Vec<(u8, u32, bool)>>,
    /// For each closed state and ASCII character, the closure with the mask 0 of the step, with [`MATCHED`] set when it has `Match`, or [`UNKNOWN`].
    fast: Vec<u32>,
    /// The steps of the closed states after the characters.
    steps: FxMap<(u32, u32), u32>,
    /// The items of [`Dfa::fast`] for the characters that are not ASCII.
    wide: FxMap<(u32, u32), u32>,
    /// The state of the start of the program and the state with no threads, when the DFA has them.
    ends: Option<(u32, u32)>,
    /// True when each step also starts a thread at the start of the program.
    restart: bool,
}

/// A step of the DFA that it did not take yet.
const UNKNOWN: u32 = u32::MAX;

/// The bit of an item of [`Dfa::fast`] for a closure that has `Match`.
const MATCHED: u32 = 1 << 31;

impl Dfa {
    fn intern(&mut self, mut pcs: Vec<usize>) -> u32 {
        pcs.sort_unstable();
        pcs.dedup();
        if let Some(&id) = self.ids.get(pcs.as_slice()) {
            return id;
        }
        let id = u32::try_from(self.states.len()).unwrap_or(0) & !MATCHED;
        let pcs: Box<[usize]> = pcs.into();
        self.ids.insert(pcs.clone(), id);
        self.states.push(pcs);
        self.closed.push(Vec::new());
        self.fast.extend([UNKNOWN; 128]);
        id
    }
}

/// An instruction of the machine.
#[derive(Clone, Copy, Debug)]
enum Inst {
    /// A character of the set with this index.
    Set(usize),
    /// A constraint.
    Assert(Assert),
    /// A lookaround constraint, with the index of its program.
    Look(Look, usize),
    /// The position into this slot of the groups.
    Save(usize),
    /// The same text as the group with this number.
    Backref(usize),
    /// Try the first target, then the second.
    Split(usize, usize),
    Jmp(usize),
    /// The position into this mark.
    Mark(usize),
    /// Fail when the position is the same as at this mark.
    Progress(usize),
    Match,
}

/// One program: the main pattern or the pattern of a lookaround.
#[derive(Debug, Default)]
struct Prog {
    insts: Vec<Inst>,
}

/// The compiled pattern.
#[derive(Debug)]
pub(super) struct Program {
    progs: Vec<Prog>,
    sets: Vec<Set>,
    groups: usize,
    marks: usize,
    backrefs: bool,
    nlanch: bool,
    icase: bool,
    /// True when a match can only start at the start of the string, because the pattern starts with `^` without `REG_NLANCH` or with `\A`.
    anchored: bool,
    /// The DFA of the main program, or `None` when the program has an instruction that a DFA cannot run: a lookaround or a back reference. The guards of empty loops only stop paths back to a pair of instruction and position that the machine tried already, so the DFA follows them as if they were not there.
    dfa: Option<RefCell<Dfa>>,
    /// The DFA that also starts a thread at each position, which finds the first end of a match that starts anywhere. It is `None` when [`Program::dfa`] is `None`.
    scan: Option<RefCell<Dfa>>,
    /// The mask of the constraints that the main program has, with the bits of [`ASSERTS`].
    asserts: u8,
}

/// The state of the compiler.
struct Compiler {
    progs: Vec<Prog>,
    sets: Vec<Set>,
    marks: usize,
    backrefs: bool,
}

impl Compiler {
    fn emit(&mut self, prog: usize, inst: Inst) -> Result<usize, Code> {
        let insts = &mut self.progs[prog].insts;
        if insts.len() >= MAX_INSTS {
            return Err(Code::TooBig);
        }
        insts.push(inst);
        Ok(insts.len() - 1)
    }

    fn here(&self, prog: usize) -> usize {
        self.progs[prog].insts.len()
    }

    fn patch(&mut self, prog: usize, at: usize, inst: Inst) {
        self.progs[prog].insts[at] = inst;
    }

    /// The instructions of a node, which a copy of a set reuses by its index.
    fn node(&mut self, prog: usize, node: &Node, set_of: &mut SetIds) -> Result<(), Code> {
        match node {
            Node::Empty => {}
            Node::Set(set) => {
                let id = set_of.get(set, &mut self.sets);
                self.emit(prog, Inst::Set(id))?;
            }
            Node::Assert(assert) => {
                self.emit(prog, Inst::Assert(*assert))?;
            }
            Node::Look(look, inner) => {
                let sub = self.progs.len();
                self.progs.push(Prog::default());
                self.node(sub, inner, set_of)?;
                self.emit(sub, Inst::Match)?;
                self.emit(prog, Inst::Look(*look, sub))?;
            }
            Node::Group(number, inner) => {
                if *number > 0 {
                    self.emit(prog, Inst::Save(2 * number))?;
                }
                self.node(prog, inner, set_of)?;
                if *number > 0 {
                    self.emit(prog, Inst::Save(2 * number + 1))?;
                }
            }
            Node::Backref(number) => {
                self.backrefs = true;
                self.emit(prog, Inst::Backref(*number))?;
            }
            Node::Concat(items) => {
                for item in items {
                    self.node(prog, item, set_of)?;
                }
            }
            Node::Alt(branches) => {
                let mut jumps = Vec::new();
                for (at, branch) in branches.iter().enumerate() {
                    if at + 1 < branches.len() {
                        let split = self.emit(prog, Inst::Split(0, 0))?;
                        self.node(prog, branch, set_of)?;
                        jumps.push(self.emit(prog, Inst::Jmp(0))?);
                        let next = self.here(prog);
                        self.patch(prog, split, Inst::Split(split + 1, next));
                    } else {
                        self.node(prog, branch, set_of)?;
                    }
                }
                let end = self.here(prog);
                for jump in jumps {
                    self.patch(prog, jump, Inst::Jmp(end));
                }
            }
            Node::Repeat(inner, min, max, _) => {
                for _ in 0..*min {
                    self.node(prog, inner, set_of)?;
                }
                if *max == DUPINF {
                    let mark = self.marks;
                    self.marks += 1;
                    let split = self.emit(prog, Inst::Split(0, 0))?;
                    self.emit(prog, Inst::Mark(mark))?;
                    self.node(prog, inner, set_of)?;
                    self.emit(prog, Inst::Progress(mark))?;
                    self.emit(prog, Inst::Jmp(split))?;
                    let end = self.here(prog);
                    self.patch(prog, split, Inst::Split(split + 1, end));
                } else {
                    let mut splits = Vec::new();
                    for _ in *min..*max {
                        splits.push(self.emit(prog, Inst::Split(0, 0))?);
                        self.node(prog, inner, set_of)?;
                    }
                    let end = self.here(prog);
                    for split in splits {
                        self.patch(prog, split, Inst::Split(split + 1, end));
                    }
                }
            }
        }
        Ok(())
    }
}

/// The index of each set of the tree, so that the copies of a repeat share one set.
#[derive(Default)]
struct SetIds {
    ids: HashMap<*const Set, usize>,
}

impl SetIds {
    fn get(&mut self, set: &Set, sets: &mut Vec<Set>) -> usize {
        *self.ids.entry(std::ptr::from_ref(set)).or_insert_with(|| {
            sets.push(set.clone().with_ascii());
            sets.len() - 1
        })
    }
}

/// One step of the search to do later.
enum Job {
    /// Try the instruction at the position.
    Try(usize, usize),
    /// Put back the old value of a slot of the groups.
    Slot(usize, Option<usize>),
    /// Put back the old value of a mark.
    Mark(usize, Option<usize>),
}

/// The state of one search in one program.
struct Search<'a> {
    program: &'a Program,
    s: &'a [u32],
    /// The results of the lookarounds by program and position: 0 for not known, 1 for false, 2 for true.
    looks: Vec<Vec<u8>>,
}

fn is_word(c: u32) -> bool {
    char::from_u32(c).is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl Program {
    /// The program of a node, with the number of the groups of the pattern and the flags after the embedded options.
    pub(super) fn new(node: &Node, groups: usize, cflags: u32) -> Result<Self, Code> {
        let mut compiler =
            Compiler { progs: vec![Prog::default()], sets: Vec::new(), marks: 0, backrefs: false };
        let mut set_of = SetIds::default();
        compiler.node(0, node, &mut set_of)?;
        compiler.emit(0, Inst::Match)?;
        let nlanch = cflags & flags::NLANCH != 0;
        let first = compiler.progs[0].insts.iter().find(|i| !matches!(i, Inst::Save(_)));
        let anchored = match first {
            Some(Inst::Assert(Assert::Bos)) => true,
            Some(Inst::Assert(Assert::Bol)) => !nlanch,
            _ => false,
        };
        let insts = &compiler.progs[0].insts;
        let plain = !insts.iter().any(|i| matches!(i, Inst::Look(..) | Inst::Backref(_)));
        let asserts = insts.iter().fold(0_u8, |mask, i| match i {
            Inst::Assert(a) => mask | 1 << ASSERTS.iter().position(|b| b == a).unwrap_or(0),
            _ => mask,
        });
        Ok(Program {
            anchored,
            dfa: plain.then(RefCell::default),
            scan: plain.then(|| RefCell::new(Dfa { restart: true, ..Dfa::default() })),
            asserts,
            progs: compiler.progs,
            sets: compiler.sets,
            groups,
            marks: compiler.marks,
            backrefs: compiler.backrefs,
            nlanch,
            icase: cflags & flags::ICASE != 0,
        })
    }

    /// [`Program::ends`] with the DFA. The threads of the backtracking machine reach the same pairs of instruction and position, so the ends are the same.
    fn dfa_ends(&self, dfa: &mut Dfa, s: &[u32], start: usize) -> Vec<bool> {
        let mut ends = vec![false; s.len() + 1];
        self.walk(dfa, s, start, |pos| {
            ends[pos] = true;
            false
        });
        ends
    }

    /// Runs a DFA on `s` from `start` and calls `hit` at each end of a match. The walk stops at the first position where `hit` gives true, and gives that position.
    fn walk(
        &self,
        dfa: &mut Dfa,
        s: &[u32],
        start: usize,
        mut hit: impl FnMut(usize) -> bool,
    ) -> Option<usize> {
        if dfa.states.len() > MAX_STATES {
            *dfa = Dfa { restart: dfa.restart, ..Dfa::default() };
        }
        let (first, dead) = match dfa.ends {
            Some(ends) => ends,
            None => {
                let ends = (dfa.intern(vec![0]), dfa.intern(Vec::new()));
                dfa.ends = Some(ends);
                ends
            }
        };
        let (mut state, matched) = self.closure(dfa, first, self.mask(s, start));
        if matched && hit(start) {
            return Some(start);
        }
        // With no constraint of words or lines, only the start and the end of the string have constraints that hold.
        let ends_only = self.asserts == 0 || (self.asserts & 0xf0 == 0 && !self.nlanch);
        for pos in start..s.len() {
            if state == dead && !dfa.restart {
                break;
            }
            let c = s[pos];
            let mask = if ends_only && pos + 1 < s.len() { 0 } else { self.mask(s, pos + 1) };
            let known = match c {
                _ if mask != 0 => UNKNOWN,
                0..128 => dfa.fast[state as usize * 128 + c as usize],
                _ => dfa.wide.get(&(state, c)).copied().unwrap_or(UNKNOWN),
            };
            let matched = if known == UNKNOWN {
                let next = self.step(dfa, state, c);
                let (closed, matched) = self.closure(dfa, next, mask);
                let known = closed | if matched { MATCHED } else { 0 };
                match c {
                    _ if mask != 0 => {}
                    0..128 => dfa.fast[state as usize * 128 + c as usize] = known,
                    _ => {
                        dfa.wide.insert((state, c), known);
                    }
                }
                state = closed;
                matched
            } else {
                state = known & !MATCHED;
                known & MATCHED != 0
            };
            if matched && hit(pos + 1) {
                return Some(pos + 1);
            }
        }
        None
    }

    /// The state of the DFA after a closed state reads a character.
    fn step(&self, dfa: &mut Dfa, closed: u32, c: u32) -> u32 {
        if let Some(&next) = dfa.steps.get(&(closed, c)) {
            return next;
        }
        let insts = &self.progs[0].insts;
        let next: Vec<usize> = dfa.states[closed as usize]
            .iter()
            .filter(|&&pc| matches!(insts[pc], Inst::Set(id) if self.sets[id].contains(c)))
            .map(|&pc| pc + 1)
            .chain(dfa.restart.then_some(0))
            .collect();
        let next = dfa.intern(next);
        dfa.steps.insert((closed, c), next);
        next
    }

    /// The closed state of a state of the DFA for a mask, and true when the closure has `Match`.
    fn closure(&self, dfa: &mut Dfa, state: u32, mask: u8) -> (u32, bool) {
        if let Some(&(_, closed, matched)) = dfa.closed[state as usize].iter().find(|c| c.0 == mask)
        {
            return (closed, matched);
        }
        let (waiting, matched) = self.close(&dfa.states[state as usize], mask);
        let closed = dfa.intern(waiting);
        dfa.closed[state as usize].push((mask, closed, matched));
        (closed, matched)
    }

    /// The mask of the constraints of the main program that hold at a position, with the bits of [`ASSERTS`].
    fn mask(&self, s: &[u32], pos: usize) -> u8 {
        if self.asserts == 0 {
            return 0;
        }
        let newline = u32::from(b'\n');
        let bos = pos == 0;
        let eos = pos == s.len();
        let bol = bos || (self.nlanch && s[pos - 1] == newline);
        let eol = eos || (self.nlanch && s[pos] == newline);
        let mut mask = u8::from(bol) | u8::from(eol) << 1 | u8::from(bos) << 2 | u8::from(eos) << 3;
        if self.asserts & 0xf0 != 0 {
            let before = !bos && is_word(s[pos - 1]);
            let after = !eos && is_word(s[pos]);
            mask |= u8::from(!before && after) << 4
                | u8::from(before && !after) << 5
                | u8::from(before != after) << 6
                | u8::from(before == after) << 7;
        }
        mask & self.asserts
    }

    /// The closure of a state of the DFA with a mask of the constraints that hold: the instructions `Set` that the threads reach without reading a character, and true when they reach `Match`.
    fn close(&self, state: &[usize], mask: u8) -> (Vec<usize>, bool) {
        let insts = &self.progs[0].insts;
        let mut seen = vec![false; insts.len()];
        let mut stack: Vec<usize> = state.to_vec();
        let mut waiting = Vec::new();
        let mut matched = false;
        while let Some(pc) = stack.pop() {
            if std::mem::replace(&mut seen[pc], true) {
                continue;
            }
            match insts[pc] {
                Inst::Set(_) => waiting.push(pc),
                Inst::Match => matched = true,
                Inst::Assert(a) => {
                    let bit = ASSERTS.iter().position(|&b| b == a).unwrap_or(0);
                    if mask & (1 << bit) != 0 {
                        stack.push(pc + 1);
                    }
                }
                Inst::Save(_) | Inst::Mark(_) | Inst::Progress(_) => stack.push(pc + 1),
                Inst::Split(first, second) => {
                    stack.push(second);
                    stack.push(first);
                }
                Inst::Jmp(to) => stack.push(to),
                Inst::Look(..) | Inst::Backref(_) => {}
            }
        }
        (waiting, matched)
    }

    /// True when a match can only start at the start of the string.
    pub(super) fn anchored(&self) -> bool {
        self.anchored
    }

    /// True when the pattern has back references.
    pub(super) fn has_backrefs(&self) -> bool {
        self.backrefs
    }

    /// The start of the leftmost match in `s` that starts at `from` or after it.
    pub(super) fn first(&self, s: &[u32], from: usize) -> Option<usize> {
        if let (Some(dfa), Some(scan)) = (&self.dfa, &self.scan) {
            // A match that ends first starts at the end or before it, so the leftmost start is not after it.
            let end = if self.anchored {
                if from > 0 {
                    return None;
                }
                0
            } else {
                self.walk(&mut scan.borrow_mut(), s, from, |_| true)?
            };
            let dfa = &mut dfa.borrow_mut();
            return (from..=end).find(|&start| self.walk(dfa, s, start, |_| true).is_some());
        }
        let mut search = Search { program: self, s, looks: vec![Vec::new(); self.progs.len()] };
        let mut visited = search.visited(0);
        let last = if self.anchored { 0 } else { s.len() };
        (from..=last).find(|&start| search.run(0, start, None, visited.as_mut(), None))
    }

    /// The ends of the matches from `start`: the item at a position is true when a match ends there.
    pub(super) fn ends(&self, s: &[u32], start: usize) -> Vec<bool> {
        if let Some(dfa) = &self.dfa {
            return self.dfa_ends(&mut dfa.borrow_mut(), s, start);
        }
        self.track_ends(s, start)
    }

    /// [`Program::ends`] with the backtracking machine.
    fn track_ends(&self, s: &[u32], start: usize) -> Vec<bool> {
        let mut search = Search { program: self, s, looks: vec![Vec::new(); self.progs.len()] };
        let mut visited = search.visited(0);
        let mut ends = vec![false; s.len() + 1];
        search.run(0, start, None, visited.as_mut(), Some(&mut ends));
        ends
    }
}

impl Search<'_> {
    /// A new set of tried pairs for a program, or none when the program has back references or the set is too large.
    fn visited(&self, prog: usize) -> Option<Vec<u64>> {
        let bits = self.program.progs[prog].insts.len().checked_mul(self.s.len() + 1)?;
        (!self.program.backrefs && bits <= MAX_VISITED).then(|| vec![0; bits.div_ceil(64)])
    }

    /// True when the constraint holds at the position.
    fn holds(&self, assert: Assert, pos: usize) -> bool {
        let s = self.s;
        let newline = u32::from(b'\n');
        let word_before = pos > 0 && is_word(s[pos - 1]);
        let word_after = pos < s.len() && is_word(s[pos]);
        match assert {
            Assert::Bol => pos == 0 || (self.program.nlanch && s[pos - 1] == newline),
            Assert::Eol => pos == s.len() || (self.program.nlanch && s[pos] == newline),
            Assert::Bos => pos == 0,
            Assert::Eos => pos == s.len(),
            Assert::WordStart => !word_before && word_after,
            Assert::WordEnd => word_before && !word_after,
            Assert::Boundary => word_before != word_after,
            Assert::NotBoundary => word_before == word_after,
        }
    }

    /// True when the lookaround holds at the position.
    fn look(&mut self, look: Look, prog: usize, pos: usize) -> bool {
        if self.looks[prog].is_empty() {
            self.looks[prog] = vec![0; self.s.len() + 1];
        }
        let known = self.looks[prog][pos];
        let found = if known != 0 {
            known == 2
        } else {
            let found = match look {
                Look::Ahead | Look::NotAhead => {
                    let mut visited = self.visited(prog);
                    self.run(prog, pos, None, visited.as_mut(), None)
                }
                Look::Behind | Look::NotBehind => {
                    let mut visited = self.visited(prog);
                    (0..=pos)
                        .rev()
                        .any(|start| self.run(prog, start, Some(pos), visited.as_mut(), None))
                }
            };
            self.looks[prog][pos] = if found { 2 } else { 1 };
            found
        };
        matches!(look, Look::Ahead | Look::Behind) == found
    }

    /// The text of a group, when it is set.
    fn group(slots: &[Option<usize>], number: usize) -> Option<(usize, usize)> {
        Some((slots.get(2 * number).copied()??, slots.get(2 * number + 1).copied()??))
    }

    /// True when the text at the position is the same as the text of the group.
    fn backref(&self, slots: &[Option<usize>], number: usize, pos: usize) -> Option<usize> {
        let (start, end) = Self::group(slots, number)?;
        let len = end.checked_sub(start)?;
        let here = self.s.get(pos..pos + len)?;
        let same = if self.program.icase {
            here.iter().zip(&self.s[start..end]).all(|(&a, &b)| to_lower(a) == to_lower(b))
        } else {
            here == &self.s[start..end]
        };
        same.then_some(pos + len)
    }

    /// The search in a program from a start position. With an end, a match must end there. With a list of ends, the search does not stop at a match: it tries all the paths and marks the end of each match in the list.
    fn run(
        &mut self,
        prog: usize,
        start: usize,
        end: Option<usize>,
        mut visited: Option<&mut Vec<u64>>,
        mut ends: Option<&mut [bool]>,
    ) -> bool {
        let program = self.program;
        let insts = &program.progs[prog].insts;
        let width = self.s.len() + 1;
        let mut slots: Vec<Option<usize>> = vec![None; 2 * program.groups + 2];
        let mut marks: Vec<Option<usize>> = vec![None; program.marks];
        let mut jobs = vec![Job::Try(0, start)];
        while let Some(job) = jobs.pop() {
            let (mut pc, mut pos) = match job {
                Job::Try(pc, pos) => (pc, pos),
                Job::Slot(slot, old) => {
                    slots[slot] = old;
                    continue;
                }
                Job::Mark(mark, old) => {
                    marks[mark] = old;
                    continue;
                }
            };
            loop {
                if let Some(visited) = visited.as_deref_mut() {
                    let bit = pc * width + pos;
                    if visited[bit / 64] & (1 << (bit % 64)) != 0 {
                        break;
                    }
                    visited[bit / 64] |= 1 << (bit % 64);
                }
                match insts[pc] {
                    Inst::Set(id) => {
                        if pos < self.s.len() && program.sets[id].contains(self.s[pos]) {
                            pc += 1;
                            pos += 1;
                        } else {
                            break;
                        }
                    }
                    Inst::Assert(assert) => {
                        if !self.holds(assert, pos) {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::Look(look, sub) => {
                        if !self.look(look, sub, pos) {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::Save(slot) => {
                        jobs.push(Job::Slot(slot, slots[slot]));
                        slots[slot] = Some(pos);
                        pc += 1;
                    }
                    Inst::Backref(number) => match self.backref(&slots, number, pos) {
                        Some(next) => {
                            pc += 1;
                            pos = next;
                        }
                        None => break,
                    },
                    Inst::Split(first, second) => {
                        jobs.push(Job::Try(second, pos));
                        pc = first;
                    }
                    Inst::Jmp(to) => pc = to,
                    Inst::Mark(mark) => {
                        jobs.push(Job::Mark(mark, marks[mark]));
                        marks[mark] = Some(pos);
                        pc += 1;
                    }
                    Inst::Progress(mark) => {
                        if marks[mark] == Some(pos) {
                            break;
                        }
                        pc += 1;
                    }
                    Inst::Match => {
                        if let Some(ends) = ends.as_deref_mut() {
                            ends[pos] = true;
                            break;
                        }
                        if end.is_none_or(|end| end == pos) {
                            return true;
                        }
                        break;
                    }
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::super::{flags, parse};
    use super::*;

    #[test]
    fn dfa_ends_are_the_ends_of_the_backtracker() {
        let patterns = [
            "a*b",
            "(a|ab)(c|bcd)",
            "^https?://(?:www\\.)?([^/]+)/.*$",
            "\\mfoo\\M",
            "\\yab\\Y",
            "^$",
            "(?n)^b$",
            "[[:alpha:]]+\\d?",
            "x{2,3}y*",
            "(a*)*",
            ".*",
            "",
        ];
        let strings = [
            "",
            "ab",
            "aab",
            "abcd",
            "foo bar",
            "afoob",
            "http://www.a.com/x",
            "a\nb",
            "xxxyy",
            "abab ab",
        ];
        for pattern in patterns {
            let chars: Vec<u32> = pattern.chars().map(u32::from).collect();
            for cflags in [flags::ADVANCED, flags::ADVANCED | flags::ICASE] {
                let parsed = parse::Parser::run(&chars, cflags).unwrap();
                let program = Program::new(&parsed.node, parsed.groups, parsed.cflags).unwrap();
                let dfa = program.dfa.as_ref().unwrap_or_else(|| panic!("{pattern} has no DFA"));
                for string in strings {
                    let s: Vec<u32> = string.chars().map(u32::from).collect();
                    let mut search = Search {
                        program: &program,
                        s: &s,
                        looks: vec![Vec::new(); program.progs.len()],
                    };
                    for from in 0..=s.len() {
                        let mut visited = search.visited(0);
                        let last = if program.anchored { 0 } else { s.len() };
                        let want = (from..=last)
                            .find(|&start| search.run(0, start, None, visited.as_mut(), None));
                        assert_eq!(
                            program.first(&s, from),
                            want,
                            "{pattern} on {string:?} after {from}"
                        );
                    }
                    for start in 0..=s.len() {
                        let want = program.track_ends(&s, start);
                        let got = program.dfa_ends(&mut dfa.borrow_mut(), &s, start);
                        assert_eq!(got, want, "{pattern} on {string:?} from {start}");
                    }
                }
            }
        }
    }
}
