//! The LALR(1) automaton.
//!
//! The steps are the classic ones. Build the LR(0) states. Compute the lookahead sets with the relations of DeRemer and Pennello (1982): `reads`, `includes` and `lookback`. Then make the action of each state and resolve the conflicts with the precedence declarations. The conflict rules copy bison (`src/conflicts.c`), so the counts can be compared with the `%expect` of the grammar.

use std::collections::HashMap;

use super::reader::{Assoc, Grammar, Sym};

/// A set of small integers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Bits(Vec<u64>);

impl Bits {
    pub(crate) fn new(size: usize) -> Bits {
        Bits(vec![0; size.div_ceil(64)])
    }

    pub(crate) fn insert(&mut self, i: usize) {
        self.0[i / 64] |= 1 << (i % 64);
    }

    pub(crate) fn remove(&mut self, i: usize) {
        self.0[i / 64] &= !(1 << (i % 64));
    }

    pub(crate) fn contains(&self, i: usize) -> bool {
        self.0[i / 64] & (1 << (i % 64)) != 0
    }

    /// Adds the members of `other`, and tells if the set changed.
    pub(crate) fn union(&mut self, other: &Bits) -> bool {
        let mut changed = false;
        for (a, b) in self.0.iter_mut().zip(&other.0) {
            let next = *a | b;
            changed |= next != *a;
            *a = next;
        }
        changed
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.0.iter().enumerate().flat_map(|(w, &word)| {
            (0..64).filter(move |b| word & (1 << b) != 0).map(move |b| w * 64 + b)
        })
    }
}

/// An item: a rule and the position of the dot in its right side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Item {
    pub(crate) rule: u32,
    pub(crate) dot: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Shift(u32),
    Reduce(u32),
    /// A `%nonassoc` conflict: the input is an error.
    Error,
    Accept,
}

#[derive(Debug)]
pub(crate) struct State {
    pub(crate) kernel: Vec<Item>,
    /// The transitions on terminals and nonterminals, sorted by symbol.
    pub(crate) transitions: Vec<(Sym, u32)>,
    /// The rules that are complete in this state, in rule order.
    pub(crate) reductions: Vec<u32>,
    /// The lookahead set of each reduction, in the same order.
    pub(crate) lookaheads: Vec<Bits>,
    /// The action on each terminal after the conflicts are resolved, sorted by terminal.
    pub(crate) actions: Vec<(Sym, Action)>,
    /// The actions that conflict resolution rejected, as bison prints them in brackets.
    pub(crate) rejected: Vec<(Sym, Action)>,
    pub(crate) sr_conflicts: usize,
    pub(crate) rr_conflicts: usize,
}

#[derive(Debug)]
pub(crate) struct Automaton {
    pub(crate) states: Vec<State>,
}

impl Automaton {
    pub(crate) fn sr_conflicts(&self) -> usize {
        self.states.iter().map(|s| s.sr_conflicts).sum()
    }

    pub(crate) fn rr_conflicts(&self) -> usize {
        self.states.iter().map(|s| s.rr_conflicts).sum()
    }

    fn goto(&self, state: u32, sym: Sym) -> Option<u32> {
        let t = &self.states[state as usize].transitions;
        t.binary_search_by_key(&sym, |&(s, _)| s).ok().map(|i| t[i].1)
    }
}

pub(crate) fn build(g: &Grammar) -> Automaton {
    let mut automaton = lr0(g);
    let lookaheads = lookaheads(g, &automaton);
    for (state, sets) in automaton.states.iter_mut().zip(lookaheads) {
        state.lookaheads = sets;
    }
    for state in &mut automaton.states {
        resolve(g, state);
    }
    automaton
}

/// The symbol after the dot, if any.
fn next_symbol(g: &Grammar, item: Item) -> Option<Sym> {
    g.rules[item.rule as usize].rhs.get(item.dot as usize).copied()
}

fn lr0(g: &Grammar) -> Automaton {
    let nsyms = g.names.len();
    let nt0 = g.terminals;
    let mut rules_of: Vec<Vec<u32>> = vec![Vec::new(); nsyms];
    for (i, rule) in g.rules.iter().enumerate() {
        rules_of[rule.lhs as usize].push(i as u32);
    }
    // first_nts[A] is the set of nonterminals B with A =>* B gamma by leftmost steps, A included. The closure of a kernel uses it.
    let mut first_nts: Vec<Bits> = (0..nsyms).map(|_| Bits::new(nsyms)).collect();
    for a in nt0..nsyms {
        first_nts[a].insert(a);
        for &r in &rules_of[a] {
            if let Some(&b) = g.rules[r as usize].rhs.first()
                && b as usize >= nt0
            {
                first_nts[a].insert(b as usize);
            }
        }
    }
    let mut changed = true;
    while changed {
        changed = false;
        for a in nt0..nsyms {
            let members: Vec<usize> = first_nts[a].iter().collect();
            for b in members {
                if b != a {
                    let other = first_nts[b].clone();
                    changed |= first_nts[a].union(&other);
                }
            }
        }
    }

    let mut states: Vec<State> = Vec::new();
    let mut index: HashMap<Vec<Item>, u32> = HashMap::new();
    let start = vec![Item { rule: 0, dot: 0 }];
    index.insert(start.clone(), 0);
    states.push(new_state(start));
    let mut next = 0;
    while next < states.len() {
        let kernel = states[next].kernel.clone();
        let mut closure = kernel.clone();
        let mut wanted = Bits::new(nsyms);
        for &item in &kernel {
            if let Some(s) = next_symbol(g, item)
                && s as usize >= nt0
            {
                wanted.union(&first_nts[s as usize]);
            }
        }
        for a in wanted.iter() {
            closure.extend(rules_of[a].iter().map(|&rule| Item { rule, dot: 0 }));
        }
        let mut by_symbol: Vec<(Sym, Item)> = Vec::new();
        let mut reductions = Vec::new();
        for item in closure {
            match next_symbol(g, item) {
                Some(s) => by_symbol.push((s, Item { rule: item.rule, dot: item.dot + 1 })),
                None => reductions.push(item.rule),
            }
        }
        by_symbol.sort();
        reductions.sort_unstable();
        reductions.dedup();
        let mut transitions = Vec::new();
        let mut i = 0;
        while i < by_symbol.len() {
            let sym = by_symbol[i].0;
            let mut target: Vec<Item> = Vec::new();
            while i < by_symbol.len() && by_symbol[i].0 == sym {
                target.push(by_symbol[i].1);
                i += 1;
            }
            target.dedup();
            let id = match index.get(&target) {
                Some(&id) => id,
                None => {
                    let id = states.len() as u32;
                    index.insert(target.clone(), id);
                    states.push(new_state(target));
                    id
                }
            };
            transitions.push((sym, id));
        }
        states[next].transitions = transitions;
        states[next].reductions = reductions;
        next += 1;
    }
    Automaton { states }
}

fn new_state(kernel: Vec<Item>) -> State {
    State {
        kernel,
        transitions: Vec::new(),
        reductions: Vec::new(),
        lookaheads: Vec::new(),
        actions: Vec::new(),
        rejected: Vec::new(),
        sr_conflicts: 0,
        rr_conflicts: 0,
    }
}

/// The lookahead set of each reduction of each state.
fn lookaheads(g: &Grammar, a: &Automaton) -> Vec<Vec<Bits>> {
    let nt0 = g.terminals;
    let nterms = g.terminals;
    // The nonterminal transitions. Each one is a node of the relations.
    let mut gotos: Vec<(u32, Sym, u32)> = Vec::new();
    let mut goto_index: HashMap<(u32, Sym), usize> = HashMap::new();
    for (s, state) in a.states.iter().enumerate() {
        for &(sym, to) in &state.transitions {
            if sym as usize >= nt0 {
                goto_index.insert((s as u32, sym), gotos.len());
                gotos.push((s as u32, sym, to));
            }
        }
    }

    let mut nullable = vec![false; g.names.len()];
    let mut changed = true;
    while changed {
        changed = false;
        for rule in &g.rules {
            if !nullable[rule.lhs as usize] && rule.rhs.iter().all(|&s| nullable[s as usize]) {
                nullable[rule.lhs as usize] = true;
                changed = true;
            }
        }
    }

    // DR and reads give Read.
    let mut read: Vec<Bits> = Vec::with_capacity(gotos.len());
    let mut reads: Vec<Vec<usize>> = vec![Vec::new(); gotos.len()];
    for (i, &(_, _, to)) in gotos.iter().enumerate() {
        let mut dr = Bits::new(nterms);
        for &(sym, _) in &a.states[to as usize].transitions {
            if (sym as usize) < nt0 {
                dr.insert(sym as usize);
            } else if nullable[sym as usize] {
                reads[i].push(goto_index[&(to, sym)]);
            }
        }
        read.push(dr);
    }
    digraph(&reads, &mut read);

    // includes and lookback.
    let mut includes: Vec<Vec<usize>> = vec![Vec::new(); gotos.len()];
    let mut lookback: HashMap<(u32, u32), Vec<usize>> = HashMap::new();
    for (j, &(from, b, _)) in gotos.iter().enumerate() {
        for (r, rule) in g.rules.iter().enumerate() {
            if rule.lhs != b {
                continue;
            }
            let mut state = from;
            let mut path = Vec::with_capacity(rule.rhs.len());
            for &sym in &rule.rhs {
                path.push(state);
                state = a.goto(state, sym).expect("each prefix of a rule has a transition");
            }
            lookback.entry((state, r as u32)).or_default().push(j);
            for (k, &sym) in rule.rhs.iter().enumerate().rev() {
                if (sym as usize) >= nt0 {
                    includes[goto_index[&(path[k], sym)]].push(j);
                }
                if !nullable[sym as usize] {
                    break;
                }
            }
        }
    }
    let mut follow = read;
    digraph(&includes, &mut follow);

    a.states
        .iter()
        .enumerate()
        .map(|(s, state)| {
            state
                .reductions
                .iter()
                .map(|&r| {
                    let mut set = Bits::new(nterms);
                    for &j in lookback.get(&(s as u32, r)).map_or(&[][..], Vec::as_slice) {
                        set.union(&follow[j]);
                    }
                    set
                })
                .collect()
        })
        .collect()
}

/// The digraph algorithm of DeRemer and Pennello: `f[x]` becomes the union of `f[y]` over all `y` reachable from `x`. The members of a cycle get the same set.
fn digraph(edges: &[Vec<usize>], f: &mut [Bits]) {
    const DONE: usize = usize::MAX;
    let n = edges.len();
    let mut depth = vec![0usize; n];
    let mut stack: Vec<usize> = Vec::new();
    // An explicit stack of (node, next edge), so a deep grammar does not overflow the thread stack.
    let mut work: Vec<(usize, usize)> = Vec::new();
    for root in 0..n {
        if depth[root] != 0 {
            continue;
        }
        stack.push(root);
        depth[root] = stack.len();
        work.push((root, 0));
        while let Some(&mut (x, ref mut e)) = work.last_mut() {
            if let Some(&y) = edges[x].get(*e) {
                *e += 1;
                if depth[y] == 0 {
                    stack.push(y);
                    depth[y] = stack.len();
                    work.push((y, 0));
                    continue;
                }
                depth[x] = depth[x].min(depth[y]);
                if y != x {
                    let fy = f[y].clone();
                    f[x].union(&fy);
                }
                continue;
            }
            work.pop();
            if let Some(&(parent, _)) = work.last() {
                depth[parent] = depth[parent].min(depth[x]);
                let fx = f[x].clone();
                f[parent].union(&fx);
            }
            if depth[x] == stack.iter().position(|&s| s == x).map_or(0, |p| p + 1) {
                loop {
                    let top = stack.pop().expect("x is on the stack");
                    depth[top] = DONE;
                    if top == x {
                        break;
                    }
                    f[top] = f[x].clone();
                }
            }
        }
    }
}

/// Makes the actions of a state and resolves its conflicts as bison does.
fn resolve(g: &Grammar, state: &mut State) {
    let nterms = g.terminals;
    let mut shifts = Bits::new(nterms);
    let mut shift_to: HashMap<Sym, u32> = HashMap::new();
    for &(sym, to) in &state.transitions {
        if (sym as usize) < nterms {
            shifts.insert(sym as usize);
            shift_to.insert(sym, to);
        }
    }
    let accept = state.kernel.iter().any(|i| i.rule == 0 && i.dot == 2);
    let mut errors: Vec<Sym> = Vec::new();

    // First pass: the precedence rules, for each reduction that has a precedence.
    for k in 0..state.reductions.len() {
        let rule = state.reductions[k] as usize;
        let rule_prec = g.rule_prec(rule);
        if rule_prec == 0 {
            continue;
        }
        let candidates: Vec<usize> =
            state.lookaheads[k].iter().filter(|&t| shifts.contains(t)).collect();
        for t in candidates {
            let (token_prec, assoc) = g.prec[t];
            if token_prec == 0 {
                continue;
            }
            let (keep_shift, keep_reduce) = if token_prec < rule_prec {
                (false, true)
            } else if token_prec > rule_prec {
                (true, false)
            } else {
                match assoc {
                    Assoc::Precedence => continue,
                    Assoc::Right => (true, false),
                    Assoc::Left => (false, true),
                    Assoc::Nonassoc => (false, false),
                }
            };
            if !keep_shift {
                shifts.remove(t);
            }
            if !keep_reduce {
                state.lookaheads[k].remove(t);
            }
            if !keep_shift && !keep_reduce {
                errors.push(t as Sym);
            }
        }
    }

    // Second pass: count what is left, as count_state_sr_conflicts and count_state_rr_conflicts do.
    let mut any_reduce = Bits::new(nterms);
    for set in &state.lookaheads {
        any_reduce.union(set);
    }
    state.sr_conflicts = any_reduce.iter().filter(|&t| shifts.contains(t)).count();
    state.rr_conflicts = (0..nterms)
        .map(|t| state.lookaheads.iter().filter(|s| s.contains(t)).count())
        .filter(|&n| n >= 2)
        .map(|n| n - 1)
        .sum();

    // The actions: a shift wins over a reduction, and the first rule wins over a later rule.
    let mut actions: Vec<(Sym, Action)> = Vec::new();
    let mut rejected: Vec<(Sym, Action)> = Vec::new();
    for t in 0..nterms {
        let sym = t as Sym;
        let mut chosen = None;
        if t == 0 && accept {
            chosen = Some(Action::Accept);
        } else if shifts.contains(t) {
            chosen = Some(Action::Shift(shift_to[&sym]));
        } else if errors.contains(&sym) {
            chosen = Some(Action::Error);
        }
        for (k, &rule) in state.reductions.iter().enumerate() {
            if state.lookaheads[k].contains(t) {
                if chosen.is_none() {
                    chosen = Some(Action::Reduce(rule));
                } else {
                    rejected.push((sym, Action::Reduce(rule)));
                }
            }
        }
        if let Some(action) = chosen {
            actions.push((sym, action));
        }
    }
    state.actions = actions;
    state.rejected = rejected;
}

#[cfg(test)]
mod tests {
    use super::super::reader::read;
    use super::{Action, Bits, build, digraph};

    #[test]
    fn digraph_gives_a_cycle_one_set() {
        let edges = vec![vec![1], vec![2], vec![0], vec![0]];
        let mut f: Vec<Bits> = (0..4)
            .map(|i| {
                let mut b = Bits::new(8);
                b.insert(i);
                b
            })
            .collect();
        digraph(&edges, &mut f);
        for set in &f[..3] {
            assert_eq!(set.iter().collect::<Vec<_>>(), [0, 1, 2]);
        }
        assert_eq!(f[3].iter().collect::<Vec<_>>(), [0, 1, 2, 3]);
    }

    #[test]
    fn a_grammar_that_is_lalr_but_not_slr() {
        // The classic example: S -> L = R | R, L -> * R | id, R -> L. SLR has a conflict on '='. LALR(1) has none. The textbook has 10 states. bison 3.8.2 gives 11, because the rule `$accept: s $end` adds the state after `$end`.
        let g = read("%token ID\n%%\ns: l '=' r | r ;\nl: '*' r | ID ;\nr: l ;\n").unwrap();
        let a = build(&g);
        assert_eq!(a.states.len(), 11);
        assert_eq!((a.sr_conflicts(), a.rr_conflicts()), (0, 0));
    }

    #[test]
    fn a_grammar_that_is_lr1_but_not_lalr() {
        // Two LR(1) states merge in LALR(1) and give a reduce/reduce conflict on D and on E. bison 3.8.2 gives the same counts.
        let g = read("%token A B C D E\n%%\ns: A x D | A y E | B x E | B y D ;\nx: C ;\ny: C ;\n")
            .unwrap();
        let a = build(&g);
        assert_eq!((a.sr_conflicts(), a.rr_conflicts()), (0, 2));
    }

    #[test]
    fn the_accept_state() {
        let g = read("%token A\n%%\ns: A ;\n").unwrap();
        let a = build(&g);
        assert!(a.states.iter().any(|s| s.actions.contains(&(0, Action::Accept))));
    }
}
