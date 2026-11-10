//! The tree of subexpressions of `regcomp.c`, `struct subre`, and the functions of `regexec.c` that find the positions of the groups of a match: `cfindloop` and the `cdissect` family.
//!
//! The parser gives a tree of nodes, and this module builds from it the tree of `parse`, `parsebranch` and `parseqatom`, with the same flags of preference. Each node of the tree has its own program, which the dissection runs in place of the DFA of the node: the program tells where a match of the node can end. A back reference in such a program is the text of its group without the constraints, as `dupnfa` and `removeconstraints` make it. Thus the program accepts more than the node, and the dissection checks each back reference.

use std::cell::RefCell;

use super::exec::{FxMap, Program, char_at, count, starts};
use super::parse::{DUPINF, Node, Parsed, to_lower};
use super::{Code, flags};

/// The flags of a node of the tree, as in `regguts.h`.
pub(super) const LONGER: u8 = 0o1;
pub(super) const SHORTER: u8 = 0o2;
const MIXED: u8 = 0o4;
const CAP: u8 = 0o10;
const BACKR: u8 = 0o20;

/// `UP`: the flags that a node gives to its parent.
fn up(f: u8) -> u8 {
    (f & (MIXED | CAP | BACKR)) | ((f << 2) & (f << 1) & MIXED)
}

/// `MESSY`: the flags that make a node need its own place in the tree.
fn messy(f: u8) -> u8 {
    f & (MIXED | CAP | BACKR)
}

/// `PREF`: the preference of a node.
fn pref(f: u8) -> u8 {
    f & (LONGER | SHORTER)
}

/// `COMBINE`: the flags of two nodes together, with the preference of the first when it has one.
fn combine(f1: u8, f2: u8) -> u8 {
    up(f1 | f2) | if pref(f1) != 0 { pref(f1) } else { pref(f2) }
}

/// The operator of a node of the tree.
#[derive(Debug)]
enum Op {
    /// `=`: a node whose inner positions do not matter.
    Leaf,
    /// `.`: the two nodes one after the other.
    Concat(Box<Sub>, Box<Sub>),
    /// `|`: one of the nodes.
    Alt(Vec<Sub>),
    /// `*`: `min` to `max` matches of the node.
    Iter(Box<Sub>, u32, u32),
    /// `(`: the capture of a node that is a capture already, as in `((x))`.
    Capture(Box<Sub>),
    /// `b`: `min` to `max` copies of the text of the group with this number.
    Backref(usize, u32, u32),
}

/// A node of the tree, `struct subre`.
#[derive(Debug)]
struct Sub {
    op: Op,
    flags: u8,
    /// The number of the group that the node captures, or 0.
    capno: usize,
    /// The part of the pattern that the node matches.
    lang: Node,
    /// The index of the program of the node, which `Tree::finish` sets.
    id: usize,
}

impl Sub {
    fn new(op: Op, flags: u8, lang: Node) -> Sub {
        Sub { op, flags, capno: 0, lang, id: 0 }
    }

    fn leaf(flags: u8, lang: Node) -> Sub {
        Sub::new(Op::Leaf, flags, lang)
    }

    fn concat(left: Sub, right: Sub, flags: u8, lang: Node) -> Sub {
        Sub::new(Op::Concat(Box::new(left), Box::new(right)), flags, lang)
    }

    /// The children of the node.
    fn children(&self) -> Vec<&Sub> {
        match &self.op {
            Op::Leaf | Op::Backref(..) => Vec::new(),
            Op::Concat(left, right) => vec![left, right],
            Op::Alt(subs) => subs.iter().collect(),
            Op::Iter(child, ..) | Op::Capture(child) => vec![child],
        }
    }
}

/// The nodes one after the other.
fn concat(items: &[Node]) -> Node {
    match items {
        [] => Node::Empty,
        [item] => item.clone(),
        _ => Node::Concat(items.to_vec()),
    }
}

/// The items of a branch.
fn items_of(node: &Node) -> &[Node] {
    match node {
        Node::Concat(items) => items,
        Node::Empty => &[],
        other => std::slice::from_ref(other),
    }
}

/// The tree of a pattern with the programs of its nodes.
#[derive(Debug)]
pub(super) struct Tree {
    root: Sub,
    progs: Vec<Program>,
    groups: usize,
    icase: bool,
    /// True in a collation of ICU, where a back reference without the case compares the cases of Unicode.
    unicode: bool,
    /// The map and the arena of the last [`Dissect`] of [`Tree::find`], which the next search uses again with no allocation.
    scratch: RefCell<Scratch>,
    /// True when the pattern has no back references, so that the program of each node accepts the text of the node and nothing more.
    exact: bool,
}

/// The ends of the programs of a dissection: the place of each program and start in the arena, and the arena.
type Scratch = (Memo, Vec<u64>);

/// The number of programs and starts that [`Memo`] keeps in a list before it uses a map.
const SHORT: usize = 32;

/// The place in the arena of the ends of each program and start of a dissection. A dissection walks few programs from few starts, so a list with no hash is the fast case, and the map is for many.
#[derive(Debug, Default)]
struct Memo {
    list: Vec<(usize, usize, usize)>,
    map: FxMap<(usize, usize), usize>,
}

impl Memo {
    fn clear(&mut self) {
        self.list.clear();
        if !self.map.is_empty() {
            self.map.clear();
        }
    }

    fn get(&self, prog: usize, begin: usize) -> Option<usize> {
        if self.map.is_empty() {
            return self.list.iter().find(|&&(p, b, _)| p == prog && b == begin).map(|item| item.2);
        }
        self.map.get(&(prog, begin)).copied()
    }

    fn insert(&mut self, prog: usize, begin: usize, at: usize) {
        if self.map.is_empty() && self.list.len() < SHORT {
            self.list.push((prog, begin, at));
            return;
        }
        self.map.extend(self.list.drain(..).map(|(p, b, at)| ((p, b), at)));
        self.map.insert((prog, begin), at);
    }
}

/// The largest arena that [`Tree::scratch`] keeps, in words.
const MAX_SCRATCH: usize = 1 << 20;

/// The state of the builder of a tree.
struct Builder {
    /// The part of the pattern in each group, by its number, for the copies of the back references.
    groups: Vec<Option<Node>>,
}

impl Builder {
    /// `parse`: the branches of an alternation.
    fn parse(&mut self, node: &Node) -> Result<Sub, Code> {
        let Node::Alt(branches) = node else { return self.branch(items_of(node)) };
        let mut flags = LONGER;
        let mut subs = Vec::new();
        for branch in branches {
            let sub = self.branch(items_of(branch))?;
            flags |= up(flags | sub.flags);
            subs.push(sub);
        }
        Ok(if messy(flags) == 0 {
            Sub::leaf(flags, node.clone())
        } else {
            Sub::new(Op::Alt(subs), flags, node.clone())
        })
    }

    /// `parsebranch` and `parseqatom`: the items of a branch, or of the rest of a branch.
    fn branch(&mut self, items: &[Node]) -> Result<Sub, Code> {
        let mut flags = 0;
        for (i, item) in items.iter().enumerate() {
            let (atom_node, min, max, qprefer) = match item {
                Node::Repeat(inner, min, max, qprefer) => (&**inner, *min, *max, *qprefer),
                Node::Assert(_) | Node::Look(..) | Node::Empty => continue,
                other => (other, 1, 1, 0),
            };
            let (atom, special) = match atom_node {
                Node::Group(number, inner) => {
                    let atom = self.parse(inner)?;
                    if *number == 0 {
                        (Some(atom), false)
                    } else {
                        if self.groups.len() <= *number {
                            self.groups.resize(*number + 1, None);
                        }
                        self.groups[*number] = Some((**inner).clone());
                        (Some(capture(atom, *number)), true)
                    }
                }
                Node::Backref(number) => {
                    (Some(Sub::new(Op::Backref(*number, 1, 1), BACKR, atom_node.clone())), true)
                }
                _ => (None, false),
            };
            let f = flags | qprefer | atom.as_ref().map_or(0, |a| a.flags);
            if !special && messy(up(f)) == 0 {
                flags = f;
                continue;
            }
            let mut atom = atom.unwrap_or_else(|| Sub::leaf(0, atom_node.clone()));
            let t_flags = combine(qprefer, atom.flags);
            // Only a bare back reference takes the quantifier itself. A group around a back reference is an atom of type `(`.
            let quantified =
                if let (Node::Backref(_), Op::Backref(number, ..)) = (atom_node, &atom.op) {
                    let number = *number;
                    atom.op = Op::Backref(number, min, max);
                    atom.flags |= combine(qprefer, atom.flags);
                    atom.lang = item.clone();
                    atom
                } else if min == 1
                    && max == 1
                    && (qprefer == 0
                        || atom.flags & (LONGER | SHORTER | MIXED) == 0
                        || qprefer == atom.flags & (LONGER | SHORTER | MIXED))
                {
                    atom
                } else if atom.flags & (CAP | BACKR) == 0 {
                    Sub::leaf(combine(qprefer, atom.flags), item.clone())
                } else if min > 0 && atom.flags & BACKR == 0 {
                    // `x{m,n}` as `x{m-1,n-1}x`, so that only the last copy captures.
                    let f = combine(qprefer, atom.flags);
                    let less = if max == DUPINF { DUPINF } else { max - 1 };
                    let head = Node::Repeat(Box::new(atom.lang.clone()), min - 1, less, 0);
                    Sub::concat(Sub::leaf(pref(f), head), atom, f, item.clone())
                } else {
                    let f = combine(qprefer, atom.flags);
                    Sub::new(Op::Iter(Box::new(atom), min, max), f, item.clone())
                };
            // The atom is the first item of the branch when nothing is before it.
            let vacuous = i == 0;
            let prefix = || Sub::leaf(flags, concat(&items[..i]));
            if i + 1 == items.len() {
                let top_flags = flags | combine(flags, quantified.flags);
                if vacuous {
                    return Ok(quantified);
                }
                return Ok(Sub::concat(prefix(), quantified, top_flags, concat(items)));
            }
            let rest = self.branch(&items[i + 1..])?;
            let t_flags = t_flags | combine(t_flags, rest.flags);
            let top_flags = flags | combine(flags, t_flags);
            if vacuous {
                return Ok(Sub::concat(quantified, rest, top_flags, concat(items)));
            }
            let lang = concat(&items[i..]);
            let t = if matches!(quantified.op, Op::Leaf)
                && matches!(rest.op, Op::Leaf)
                && messy(up(quantified.flags | rest.flags)) == 0
            {
                Sub::leaf(combine(quantified.flags, rest.flags), lang)
            } else {
                Sub::concat(quantified, rest, t_flags, lang)
            };
            return Ok(Sub::concat(prefix(), t, top_flags, concat(items)));
        }
        Ok(Sub::leaf(flags, concat(items)))
    }

    /// The node for the programs: each back reference becomes the text of its group without the constraints.
    fn approx(&self, node: &Node, strip: bool) -> Node {
        match node {
            Node::Assert(_) | Node::Look(..) if strip => Node::Empty,
            Node::Empty | Node::Set(_) | Node::Assert(_) | Node::Look(..) => node.clone(),
            Node::Group(number, inner) => Node::Group(*number, Box::new(self.approx(inner, strip))),
            Node::Backref(number) => match self.groups.get(*number).and_then(Option::as_ref) {
                Some(group) => Node::Group(0, Box::new(self.approx(group, true))),
                // The group of a back reference after `{0}` matches nothing.
                None => Node::Set(Box::default()),
            },
            Node::Concat(items) => {
                Node::Concat(items.iter().map(|n| self.approx(n, strip)).collect())
            }
            Node::Alt(items) => Node::Alt(items.iter().map(|n| self.approx(n, strip)).collect()),
            Node::Repeat(inner, min, max, qprefer) => {
                Node::Repeat(Box::new(self.approx(inner, strip)), *min, *max, *qprefer)
            }
        }
    }

    /// `numst` and `nfatree`: the number of each node and its program.
    fn finish(&self, sub: &mut Sub, progs: &mut Vec<Program>, parsed: &Parsed) -> Result<(), Code> {
        sub.id = progs.len();
        progs.push(Program::new(&self.approx(&sub.lang, false), parsed.groups, parsed.cflags)?);
        match &mut sub.op {
            Op::Leaf | Op::Backref(..) => {}
            Op::Concat(left, right) => {
                self.finish(left, progs, parsed)?;
                self.finish(right, progs, parsed)?;
            }
            Op::Alt(subs) => {
                for sub in subs {
                    self.finish(sub, progs, parsed)?;
                }
            }
            Op::Iter(child, ..) | Op::Capture(child) => self.finish(child, progs, parsed)?,
        }
        Ok(())
    }
}

/// The capture of a group: the node itself, or a wrapper when the node captures already.
fn capture(mut atom: Sub, number: usize) -> Sub {
    if atom.capno == 0 {
        atom.flags |= CAP;
        atom.capno = number;
        return atom;
    }
    let (flags, lang) = (atom.flags | CAP, atom.lang.clone());
    let mut wrapper = Sub::new(Op::Capture(Box::new(atom)), flags, lang);
    wrapper.capno = number;
    wrapper
}

/// The positions of a match: the start and the end of the whole match first, then of each group, or `None` for a group that is not in the match.
pub(super) type Groups = Vec<Option<(usize, usize)>>;

impl Tree {
    /// The tree of a pattern.
    pub(super) fn new(parsed: &Parsed) -> Result<Tree, Code> {
        let mut builder = Builder { groups: Vec::new() };
        let mut root = builder.parse(&parsed.node)?;
        let mut progs = Vec::new();
        builder.finish(&mut root, &mut progs, parsed)?;
        fn backrefs(sub: &Sub) -> bool {
            matches!(sub.op, Op::Backref(..)) || sub.children().into_iter().any(backrefs)
        }
        let exact = !backrefs(&root);
        Ok(Tree {
            exact,
            root,
            progs,
            groups: parsed.groups,
            icase: parsed.cflags & flags::ICASE != 0,
            unicode: parsed.cflags & flags::UNICODE != 0,
            scratch: RefCell::default(),
        })
    }

    /// `cfindloop`: the leftmost match from `from` for a pattern with back references. The search finds the first end of a match of the program of the root, then tries each start up to that end. When no start gives a match, the search goes on after the end, but only while that position is before the end of the text, so a match that starts at the end of the text is found only by the first search.
    pub(super) fn cfind(&self, s: &[u8], from: usize) -> Option<Groups> {
        let mut d = Dissect::new(self, s, Vec::new());
        let mut close = from;
        loop {
            let mut first: Option<usize> = None;
            for begin in (close..=s.len()).filter(|&begin| starts(s, begin)) {
                if first.is_some_and(|end| begin > end) {
                    break;
                }
                let limit = first.unwrap_or(s.len());
                if let Some(end) = d.shortest(&self.root, begin, begin, limit) {
                    first = Some(end);
                }
            }
            let end = first?;
            if let Some(groups) = (close..=end)
                .filter(|&begin| starts(s, begin))
                .find_map(|begin| self.find(s, begin))
            {
                return Some(groups);
            }
            close = if end < s.len() { end + char_at(s, end).1 } else { end + 1 };
            if close >= s.len() {
                return None;
            }
        }
    }

    /// `cfindloop`: the positions of the leftmost match that starts at `begin`, the longest or the shortest as the root prefers.
    pub(super) fn find(&self, s: &[u8], begin: usize) -> Option<Groups> {
        let mut groups = Groups::new();
        self.find_into(s, begin, &mut groups).then_some(groups)
    }

    /// [`Tree::find`] into `groups`, which the search uses again with no allocation. When it gives false, `groups` has no meaning.
    pub(super) fn find_into(&self, s: &[u8], begin: usize, groups: &mut Groups) -> bool {
        let (mut ends, mut arena) = std::mem::take(&mut *self.scratch.borrow_mut());
        ends.clear();
        arena.clear();
        let mut pmatch = std::mem::take(groups);
        pmatch.clear();
        pmatch.resize(self.groups + 1, None);
        let mut d = Dissect { tree: self, s, ends, arena, pmatch };
        let found = self.find_in(&mut d, begin);
        *groups = d.pmatch;
        if d.arena.capacity() <= MAX_SCRATCH {
            *self.scratch.borrow_mut() = (d.ends, d.arena);
        }
        found
    }

    /// [`Tree::find_into`] with the state of a dissection. The positions are in [`Dissect::pmatch`] when it gives true.
    fn find_in<'a>(&'a self, d: &mut Dissect<'a>, begin: usize) -> bool {
        let s = d.s;
        let root = &self.root;
        let shorter = root.flags & SHORTER != 0;
        let (mut estart, mut estop) = (begin, s.len());
        loop {
            let end = if shorter {
                d.shortest(root, begin, estart, estop)
            } else {
                d.longest(root, begin, estop)
            };
            let Some(end) = end else { return false };
            d.pmatch.fill(None);
            if d.dissect(root, begin, end) {
                d.pmatch[0] = Some((begin, end));
                return true;
            }
            if shorter {
                if end == estop {
                    return false;
                }
                estart = end + 1;
            } else {
                if end == begin {
                    return false;
                }
                estop = end - 1;
            }
        }
    }
}

/// The state of the dissection of one match.
struct Dissect<'a> {
    tree: &'a Tree,
    /// The text in UTF-8. A position is the byte where a character starts.
    s: &'a [u8],
    /// The ends of the matches of each program from each start.
    ends: Memo,
    /// The ends of each program and start in [`Dissect::ends`], one bit for each position of the text, from the word that the map gives.
    arena: Vec<u64>,
    pmatch: Groups,
}

impl<'a> Dissect<'a> {
    fn new(tree: &'a Tree, s: &'a [u8], pmatch: Groups) -> Self {
        let ends = Memo::default();
        let arena = Vec::with_capacity(8 * words(s.len()));
        Dissect { tree, s, ends, arena, pmatch }
    }

    /// The number of characters from the position `from` up to the position `to`.
    fn dist(&self, from: usize, to: usize) -> usize {
        count(&self.s[from..to])
    }

    /// The ends of the matches of a node from a start, one bit for each position.
    fn ends(&mut self, sub: &Sub, begin: usize) -> &[u64] {
        let width = words(self.s.len());
        let at = match self.ends.get(sub.id, begin) {
            Some(at) => at,
            None => {
                let at = self.arena.len();
                self.arena.resize(at + width, 0);
                self.tree.progs[sub.id].ends(self.s, begin, &mut self.arena[at..]);
                self.ends.insert(sub.id, begin, at);
                at
            }
        };
        &self.arena[at..at + width]
    }

    /// `longest`: the last end of a match of the node from `begin` that is not after `limit`.
    fn longest(&mut self, sub: &Sub, begin: usize, limit: usize) -> Option<usize> {
        if limit < begin {
            return None;
        }
        let ends = self.ends(sub, begin);
        let (low, high) = (begin / 64, limit / 64);
        for at in (low..=high).rev() {
            let mut word = ends[at];
            if at == high {
                word &= u64::MAX >> (63 - limit % 64);
            }
            if at == low {
                word &= u64::MAX << (begin % 64);
            }
            if word != 0 {
                return Some(at * 64 + 63 - word.leading_zeros() as usize);
            }
        }
        None
    }

    /// `shortest`: the first end of a match of the node from `begin` from `min` to `max`.
    fn shortest(&mut self, sub: &Sub, begin: usize, min: usize, max: usize) -> Option<usize> {
        if max < min {
            return None;
        }
        let ends = self.ends(sub, begin);
        let (low, high) = (min / 64, max / 64);
        for (at, &word) in ends.iter().enumerate().take(high + 1).skip(low) {
            let mut word = word;
            if at == high {
                word &= u64::MAX >> (63 - max % 64);
            }
            if at == low {
                word &= u64::MAX << (min % 64);
            }
            if word != 0 {
                return Some(at * 64 + word.trailing_zeros() as usize);
            }
        }
        None
    }

    /// True when the node matches the text from `begin` to `end`.
    fn spans(&mut self, sub: &Sub, begin: usize, end: usize) -> bool {
        self.ends(sub, begin)[end / 64] >> (end % 64) & 1 != 0
    }

    /// `zaptreesubs`: no positions for the groups of the node.
    fn zap(&mut self, sub: &Sub) {
        if sub.capno > 0 {
            self.pmatch[sub.capno] = None;
        }
        for child in sub.children() {
            self.zap(child);
        }
    }

    /// `cdissect`: true when the node matches the text from `begin` to `end`, with the positions of its groups. The caller has checked the program of the node for the text.
    fn dissect(&mut self, sub: &'a Sub, begin: usize, end: usize) -> bool {
        let found = match &sub.op {
            Op::Leaf => true,
            Op::Backref(number, min, max) => self.backref(*number, (*min, *max), begin, end),
            Op::Concat(left, right) if left.flags & SHORTER != 0 => {
                self.rev_concat(left, right, begin, end)
            }
            Op::Concat(left, right) => self.concat(left, right, begin, end),
            Op::Alt(subs) => subs.iter().any(|sub| {
                (self.checks(sub) || self.spans(sub, begin, end)) && self.dissect(sub, begin, end)
            }),
            Op::Iter(child, min, max) if child.flags & SHORTER != 0 => {
                self.rev_iter(child, (*min, *max), begin, end)
            }
            Op::Iter(child, min, max) => self.iter(child, (*min, *max), begin, end),
            Op::Capture(child) => self.dissect(child, begin, end),
        };
        if found && sub.capno > 0 {
            self.pmatch[sub.capno] = Some((begin, end));
        }
        found
    }

    /// True when [`Dissect::dissect`] of the node checks the text itself, so that the caller can skip the walk of the program of the node: the node is a concatenation or an alternation, or the capture of one, and the pattern has no back references. Then the dissection gives true when the program of the node matches the text, and when it gives false, it writes no group, as the caller would not have called it after the walk.
    fn checks(&self, sub: &Sub) -> bool {
        self.tree.exact
            && match &sub.op {
                Op::Concat(..) | Op::Alt(..) => true,
                Op::Capture(child) => self.checks(child),
                _ => false,
            }
    }

    /// The step of `ccondissect` and `crevcondissect` at a split `mid`: true when the right part matches from `mid` to `end` and the left part from `begin` to `mid`, with the positions of their groups. When the right part checks its text, it goes first and its dissection is the check. The parts write different groups, so the order does not change the groups.
    fn split(
        &mut self,
        left: &'a Sub,
        right: &'a Sub,
        begin: usize,
        mid: usize,
        end: usize,
    ) -> bool {
        if self.checks(right) {
            if self.dissect(right, mid, end) {
                if self.dissect(left, begin, mid) {
                    return true;
                }
                self.zap(right);
            }
        } else if self.spans(right, mid, end) && self.dissect(left, begin, mid) {
            if self.dissect(right, mid, end) {
                return true;
            }
            self.zap(left);
        }
        false
    }

    /// `ccondissect`: the longest left part first.
    fn concat(&mut self, left: &'a Sub, right: &'a Sub, begin: usize, end: usize) -> bool {
        let Some(mut mid) = self.longest(left, begin, end) else { return false };
        loop {
            if self.split(left, right, begin, mid, end) {
                return true;
            }
            if mid == begin {
                return false;
            }
            match self.longest(left, begin, mid - 1) {
                Some(next) => mid = next,
                None => return false,
            }
        }
    }

    /// `crevcondissect`: the shortest left part first.
    fn rev_concat(&mut self, left: &'a Sub, right: &'a Sub, begin: usize, end: usize) -> bool {
        let Some(mut mid) = self.shortest(left, begin, begin, end) else { return false };
        loop {
            if self.split(left, right, begin, mid, end) {
                return true;
            }
            if mid == end {
                return false;
            }
            match self.shortest(left, begin, mid + 1, end) {
                Some(next) => mid = next,
                None => return false,
            }
        }
    }

    /// `cbrdissect`: the text is `min` to `max` copies of the text of the group.
    fn backref(&self, number: usize, (min, max): (u32, u32), begin: usize, end: usize) -> bool {
        let Some((so, eo)) = self.pmatch.get(number).copied().flatten() else { return false };
        if self.tree.icase {
            // The copies of a group without the case can have other bytes than the group, so the check counts characters. With the case, the copies have the bytes of the group.
            let group = chars(&self.s[so..eo]);
            let text = chars(&self.s[begin..end]);
            return copies(&group, &text, (min, max), self.tree.icase, self.tree.unicode);
        }
        copies(&self.s[so..eo], &self.s[begin..end], (min, max), self.tree.icase, self.tree.unicode)
    }
}

/// The characters of the UTF-8 text `s`.
fn chars(s: &[u8]) -> Vec<u32> {
    let mut out = Vec::with_capacity(s.len());
    let mut at = 0;
    while at < s.len() {
        let (c, width) = char_at(s, at);
        out.push(c);
        at += width;
    }
    out
}

/// `cbrdissect`: true when the characters `text` are `min` to `max` copies of the characters `group`, without the case with `icase`.
fn copies<T: Copy + Into<u32> + PartialEq>(
    group: &[T],
    text: &[T],
    (min, max): (u32, u32),
    icase: bool,
    unicode: bool,
) -> bool {
    let len = group.len();
    if len == 0 {
        return text.is_empty();
    }
    if text.is_empty() {
        return min == 0;
    }
    if !text.len().is_multiple_of(len) {
        return false;
    }
    let reps = text.len() / len;
    if reps < min as usize || (max != DUPINF && reps > max as usize) {
        return false;
    }
    text.chunks(len).all(|copy| {
        if icase {
            copy.iter()
                .zip(group)
                .all(|(&a, &b)| to_lower(a.into(), unicode) == to_lower(b.into(), unicode))
        } else {
            copy == group
        }
    })
}

impl<'a> Dissect<'a> {
    /// The check of the matches of an iteration from `first` to `last` with the ends in `endpts`. The return value is the number of the first match that fails, or `None` when all hold.
    fn verify(
        &mut self,
        child: &'a Sub,
        endpts: &[usize],
        first: usize,
        last: usize,
    ) -> Option<usize> {
        for i in first..=last {
            self.zap(child);
            if !self.dissect(child, endpts[i - 1], endpts[i]) {
                return Some(i);
            }
        }
        None
    }

    /// `citerdissect`: the longest matches of the child first.
    fn iter(&mut self, child: &'a Sub, (min, max): (u32, u32), begin: usize, end: usize) -> bool {
        let min_matches = (min as usize).max(1);
        let max_matches = bounded(self.dist(begin, end), max).max(min_matches);
        let mut endpts = vec![begin; max_matches + 1];
        let mut nverified = 0;
        let mut k = 1;
        let mut limit = end;
        while k > 0 {
            if let Some(p) = self.longest(child, endpts[k - 1], limit) {
                endpts[k] = p;
                nverified = nverified.min(k - 1);
                if p != end {
                    if k >= max_matches {
                        k -= 1;
                    } else if !(p == endpts[k - 1]
                        && (k >= min_matches || min_matches - k < self.dist(p, end)))
                    {
                        k += 1;
                        limit = end;
                        continue;
                    }
                } else if k >= min_matches {
                    match self.verify(child, &endpts, nverified + 1, k) {
                        None => return true,
                        Some(i) => {
                            nverified = i - 1;
                            k = i;
                        }
                    }
                }
            } else {
                k -= 1;
            }
            while k > 0 {
                let prev_end = endpts[k - 1];
                if endpts[k] > prev_end {
                    limit = endpts[k] - 1;
                    if limit > prev_end
                        || (k < min_matches && min_matches - k >= self.dist(prev_end, end))
                    {
                        break;
                    }
                }
                k -= 1;
            }
        }
        min == 0 && begin == end
    }

    /// `creviterdissect`: the shortest matches of the child first.
    fn rev_iter(
        &mut self,
        child: &'a Sub,
        (min, max): (u32, u32),
        begin: usize,
        end: usize,
    ) -> bool {
        if min == 0 && begin == end {
            return true;
        }
        let min_matches = (min as usize).max(1);
        let max_matches = bounded(self.dist(begin, end), max).max(min_matches);
        let mut endpts = vec![begin; max_matches + 1];
        let mut nverified = 0;
        let mut k = 1;
        let mut limit = begin;
        while k > 0 {
            if limit == endpts[k - 1]
                && limit != end
                && (k >= min_matches || min_matches - k < self.dist(limit, end))
            {
                limit += char_at(self.s, limit).1;
            }
            if k >= max_matches {
                limit = end;
            }
            if let Some(p) = self.shortest(child, endpts[k - 1], limit, end) {
                endpts[k] = p;
                nverified = nverified.min(k - 1);
                if p != end {
                    if k >= max_matches {
                        k -= 1;
                    } else {
                        k += 1;
                        limit = endpts[k - 1];
                        continue;
                    }
                } else if k >= min_matches {
                    match self.verify(child, &endpts, nverified + 1, k) {
                        None => return true,
                        Some(i) => {
                            nverified = i - 1;
                            k = i;
                        }
                    }
                }
            } else {
                k -= 1;
            }
            while k > 0 {
                if endpts[k] < end {
                    limit = endpts[k] + char_at(self.s, endpts[k]).1;
                    break;
                }
                k -= 1;
            }
        }
        false
    }
}

/// The number of words of a set of bits with a bit for each position of a text of `len` bytes, from 0 to `len`.
pub(super) fn words(len: usize) -> usize {
    len / 64 + 1
}

/// The number of matches up to `max`, where `max` can be `DUPINF`.
fn bounded(n: usize, max: u32) -> usize {
    if max == DUPINF { n } else { n.min(max as usize) }
}
