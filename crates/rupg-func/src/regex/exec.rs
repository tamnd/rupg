//! The matcher: the tree of the parser compiled to a program of a backtracking machine.
//!
//! A program with no back reference keeps a set of the pairs of instruction and position that it has tried, so each pair is tried once and the search takes time in proportion to the size of the program and the length of the string. A program with back references cannot do this, because the result then depends on the groups, and it stops an empty loop with a mark of the position at the start of each pass.

use std::collections::HashMap;

use super::parse::{Assert, DUPINF, Look, Node, Parsed, Set, to_lower};
use super::{Code, flags};

/// The largest number of instructions of a program, after the copies of the counted repeats.
const MAX_INSTS: usize = 1 << 20;

/// The largest set of tried pairs, in bits.
const MAX_VISITED: usize = 1 << 26;

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
            Node::Repeat(inner, min, max) => {
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
            sets.push(set.clone());
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
    /// The program of a tree.
    pub(super) fn compile(parsed: &Parsed) -> Result<Self, Code> {
        let mut compiler =
            Compiler { progs: vec![Prog::default()], sets: Vec::new(), marks: 0, backrefs: false };
        let mut set_of = SetIds::default();
        compiler.node(0, &parsed.node, &mut set_of)?;
        compiler.emit(0, Inst::Match)?;
        Ok(Program {
            progs: compiler.progs,
            sets: compiler.sets,
            groups: parsed.groups,
            marks: compiler.marks,
            backrefs: compiler.backrefs,
            nlanch: parsed.cflags & flags::NLANCH != 0,
            icase: parsed.cflags & flags::ICASE != 0,
        })
    }

    /// True when the pattern matches a part of `s`.
    pub(super) fn search(&self, s: &[u32]) -> bool {
        let mut search = Search { program: self, s, looks: vec![Vec::new(); self.progs.len()] };
        let mut visited = search.visited(0);
        (0..=s.len()).any(|start| search.run(0, start, None, visited.as_mut()))
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
                    self.run(prog, pos, None, visited.as_mut())
                }
                Look::Behind | Look::NotBehind => {
                    let mut visited = self.visited(prog);
                    (0..=pos).rev().any(|start| self.run(prog, start, Some(pos), visited.as_mut()))
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

    /// The search in a program from a start position. With an end, a match must end there.
    fn run(
        &mut self,
        prog: usize,
        start: usize,
        end: Option<usize>,
        mut visited: Option<&mut Vec<u64>>,
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
