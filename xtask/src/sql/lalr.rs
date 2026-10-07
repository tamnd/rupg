//! The LALR(1) table generator for the PostgreSQL grammar.
//!
//! The steps are the steps of bison 2.3, the version that PostgreSQL supports as its oldest. The LR(0) states come first, in the same order as bison makes them. The lookahead sets come from the relations of DeRemer and Pennello. The conflicts are resolved with the precedence declarations, and the tables are packed into one array with a base for each row. Because each step does what bison does, the generated tables have the state numbers of `bison -v` for the same grammar, and a difference between the two is a bug here.
//!
//! Lifted from `xtask/src/postgres/lalr.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use std::collections::HashMap;

use super::gram::{Assoc, Grammar};

/// A set of tokens, one bit for each.
#[derive(Clone)]
struct Tokens {
    words: Vec<u64>,
}

impl Tokens {
    fn new(tokens: usize) -> Self {
        Self { words: vec![0; tokens.div_ceil(64)] }
    }

    fn insert(&mut self, token: usize) {
        self.words[token / 64] |= 1 << (token % 64);
    }

    fn remove(&mut self, token: usize) {
        self.words[token / 64] &= !(1 << (token % 64));
    }

    fn contains(&self, token: usize) -> bool {
        self.words[token / 64] & (1 << (token % 64)) != 0
    }

    fn union(&mut self, other: &Self) {
        for (a, b) in self.words.iter_mut().zip(&other.words) {
            *a |= b;
        }
    }

    fn disjoint(&self, other: &Self) -> bool {
        self.words.iter().zip(&other.words).all(|(a, b)| a & b == 0)
    }

    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(|(at, &word)| {
            (0..64).filter(move |bit| word & (1 << bit) != 0).map(move |bit| at * 64 + bit)
        })
    }
}

/// One LR(0) state.
struct State {
    /// The kernel, as item numbers. An item is a position in `Items::symbols`.
    kernel: Vec<u32>,
    /// The transitions, sorted by symbol: the symbol and the state it goes to.
    transitions: Vec<(usize, usize)>,
    /// The rules that this state can reduce, in rule order.
    reductions: Vec<usize>,
    /// The lookahead set of each reduction, or `None` when the state is consistent and reduces without a lookahead.
    lookaheads: Option<Vec<Tokens>>,
}

/// The right sides of all rules in one array, as bison has them. Each rule is its symbols, then the marker `-1 - rule`. An item is an index into this array.
struct Items {
    symbols: Vec<i32>,
    /// The index of the first item of each rule.
    start: Vec<u32>,
}

/// The packed tables, ready to write.
pub(super) struct Tables {
    pub(super) states: usize,
    pub(super) final_state: usize,
    /// For each state, the base of its action row in `table`, or `None` when the state takes its default action without a lookahead.
    pub(super) action_base: Vec<Option<i32>>,
    /// For each state, the rule it reduces when the row has no entry, or 0 for an error.
    pub(super) default_reduction: Vec<u16>,
    /// For each nonterminal, the base of its goto column in `table`, or `None` when every goto on it is the default.
    pub(super) goto_base: Vec<Option<i32>>,
    /// For each nonterminal, the state of the most common goto.
    pub(super) default_goto: Vec<u16>,
    /// The entries. In an action row, a positive entry shifts to that state, a negative entry reduces by that rule and `i16::MIN` is an error. In a goto column, the entry is a state.
    pub(super) table: Vec<i16>,
    /// For each entry, the token of an action row or the state of a goto column, or -1 for none.
    pub(super) check: Vec<i16>,
}

/// The entries of one action row or goto column: the token or the state, and the value.
type Row = Vec<(usize, i32)>;

/// The value of an explicit error in a row, which a `%nonassoc` token makes.
const ERROR: i32 = i32::MIN;

/// Makes the tables of a grammar. Fails when the conflicts that remain after precedence are not the number that `%expect` gives.
pub(super) fn tables(grammar: &Grammar) -> Result<Tables, String> {
    let tokens = grammar.tokens;
    let symbols = grammar.symbols.len();
    let nonterminals = symbols - tokens;
    let rules = &grammar.rules;

    let mut items = Items { symbols: Vec::new(), start: Vec::new() };
    for (number, rule) in rules.iter().enumerate() {
        items.start.push(items.symbols.len() as u32);
        items.symbols.extend(rule.rhs.iter().map(|&s| s as i32));
        items.symbols.push(-1 - number as i32);
    }
    let mut by_lhs = vec![Vec::new(); nonterminals];
    for (number, rule) in rules.iter().enumerate() {
        by_lhs[rule.lhs - tokens].push(number);
    }

    // The nonterminals that derive the empty string.
    let mut nullable = vec![false; nonterminals];
    loop {
        let mut changed = false;
        for rule in rules {
            if !nullable[rule.lhs - tokens]
                && rule.rhs.iter().all(|&s| s >= tokens && nullable[s - tokens])
            {
                nullable[rule.lhs - tokens] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // For each nonterminal, the rules of every nonterminal that can start it, itself included. These are the rules whose first item joins a closure.
    let mut starts = vec![vec![false; nonterminals]; nonterminals];
    for (a, row) in starts.iter_mut().enumerate() {
        let mut stack = vec![a];
        row[a] = true;
        while let Some(b) = stack.pop() {
            for &rule in &by_lhs[b] {
                if let Some(&first) = rules[rule].rhs.first()
                    && first >= tokens
                    && !row[first - tokens]
                {
                    row[first - tokens] = true;
                    stack.push(first - tokens);
                }
            }
        }
    }
    let derives: Vec<Vec<u32>> = starts
        .iter()
        .map(|row| {
            let mut firsts: Vec<u32> = (0..nonterminals)
                .filter(|&b| row[b])
                .flat_map(|b| by_lhs[b].iter().map(|&rule| items.start[rule]))
                .collect();
            firsts.sort_unstable();
            firsts
        })
        .collect();

    // The LR(0) states. A state's successors are made in the order of their symbols, and the states are visited in the order they are made, as bison does.
    let mut states: Vec<State> = Vec::new();
    let mut known: HashMap<Vec<u32>, usize> = HashMap::new();
    let mut final_state = None;
    let initial = vec![items.start[0]];
    known.insert(initial.clone(), 0);
    states.push(State {
        kernel: initial,
        transitions: Vec::new(),
        reductions: Vec::new(),
        lookaheads: None,
    });
    let mut in_closure = vec![false; items.symbols.len()];
    let mut next: Vec<Vec<u32>> = vec![Vec::new(); symbols];
    let mut at = 0;
    while at < states.len() {
        let closure = closure(&states[at].kernel, &items, &derives, tokens, &mut in_closure);
        let mut shifted = Vec::new();
        let mut reductions = Vec::new();
        for &item in &closure {
            let symbol = items.symbols[item as usize];
            if symbol < 0 {
                reductions.push((-1 - symbol) as usize);
                continue;
            }
            let symbol = symbol as usize;
            if next[symbol].is_empty() {
                shifted.push(symbol);
            }
            next[symbol].push(item + 1);
        }
        shifted.sort_unstable();
        let mut transitions = Vec::with_capacity(shifted.len());
        for symbol in shifted {
            let kernel = std::mem::take(&mut next[symbol]);
            let target = match known.get(&kernel) {
                Some(&state) => state,
                None => {
                    let state = states.len();
                    if symbol == 0 {
                        final_state = Some(state);
                    }
                    known.insert(kernel.clone(), state);
                    states.push(State {
                        kernel,
                        transitions: Vec::new(),
                        reductions: Vec::new(),
                        lookaheads: None,
                    });
                    state
                }
            };
            transitions.push((symbol, target));
        }
        states[at].transitions = transitions;
        states[at].reductions = reductions;
        at += 1;
    }
    let final_state = final_state.ok_or("no state accepts")?;

    lookaheads(grammar, &mut states, &nullable, &by_lhs);
    let (actions, defaults) = actions(grammar, &mut states, grammar.expect)?;

    // The goto columns: for each nonterminal, the target from each state that has a transition on it, and the most common target as the default.
    let mut columns: Vec<Vec<(usize, usize)>> = vec![Vec::new(); nonterminals];
    for (from, state) in states.iter().enumerate() {
        for &(symbol, to) in &state.transitions {
            if symbol >= tokens {
                columns[symbol - tokens].push((from, to));
            }
        }
    }
    let mut default_goto = vec![0u16; nonterminals];
    let mut counts = vec![0usize; states.len()];
    let mut goto_rows = Vec::with_capacity(nonterminals);
    for (nonterminal, column) in columns.iter().enumerate() {
        let mut best = None;
        for &(_, to) in column {
            counts[to] += 1;
        }
        for &(_, to) in column {
            let count = counts[to];
            if best.is_none_or(|(c, s)| count > c || (count == c && to < s)) {
                best = Some((count, to));
            }
        }
        for &(_, to) in column {
            counts[to] = 0;
        }
        let default = best.map_or(usize::MAX, |(_, s)| s);
        default_goto[nonterminal] = best.map_or(0, |(_, s)| s as u16);
        goto_rows.push(
            column
                .iter()
                .filter(|&&(_, to)| to != default)
                .map(|&(from, to)| (from, to as i32))
                .collect::<Vec<_>>(),
        );
    }

    let (bases, table, check) = pack(&actions, &goto_rows, states.len());
    let action_base = bases[..states.len()].to_vec();
    let goto_base = bases[states.len()..].to_vec();
    if states.len() > i16::MAX as usize || rules.len() > i16::MAX as usize {
        return Err("the grammar has too many states or rules for 16-bit tables".to_string());
    }
    Ok(Tables {
        states: states.len(),
        final_state,
        action_base,
        default_reduction: defaults,
        goto_base,
        default_goto,
        table: table.iter().map(|&v| if v == ERROR { i16::MIN } else { v as i16 }).collect(),
        check: check.iter().map(|&c| c as i16).collect(),
    })
}

/// The closure of a kernel, sorted by item number.
fn closure(
    kernel: &[u32],
    items: &Items,
    derives: &[Vec<u32>],
    tokens: usize,
    in_closure: &mut [bool],
) -> Vec<u32> {
    let mut out: Vec<u32> = kernel.to_vec();
    for &item in kernel {
        in_closure[item as usize] = true;
    }
    for &item in kernel {
        let symbol = items.symbols[item as usize];
        if symbol >= tokens as i32 {
            for &first in &derives[symbol as usize - tokens] {
                if !in_closure[first as usize] {
                    in_closure[first as usize] = true;
                    out.push(first);
                }
            }
        }
    }
    for &item in &out {
        in_closure[item as usize] = false;
    }
    out.sort_unstable();
    out
}

/// Computes the lookahead set of each reduction in each state that needs one, with the relations of DeRemer and Pennello, as bison's `lalr.c` does.
fn lookaheads(grammar: &Grammar, states: &mut [State], nullable: &[bool], by_lhs: &[Vec<usize>]) {
    let tokens = grammar.tokens;
    let rules = &grammar.rules;

    // A state needs lookaheads when it can reduce by more than one rule, or by one rule and also shift a token.
    for state in states.iter_mut() {
        let shifts = state.transitions.first().is_some_and(|&(s, _)| s < tokens);
        if state.reductions.len() > 1 || (state.reductions.len() == 1 && shifts) {
            state.lookaheads = Some(vec![Tokens::new(tokens); state.reductions.len()]);
        }
    }

    // The nonterminal transitions, numbered in the order of their symbol and then of their state.
    let mut gotos: Vec<(usize, usize, usize)> = Vec::new();
    for (from, state) in states.iter().enumerate() {
        for &(symbol, to) in &state.transitions {
            if symbol >= tokens {
                gotos.push((symbol, from, to));
            }
        }
    }
    gotos.sort_unstable();
    let index: HashMap<(usize, usize), usize> =
        gotos.iter().enumerate().map(|(g, &(symbol, from, _))| ((from, symbol), g)).collect();
    let target = |from: usize, symbol: usize| -> usize {
        let transitions = &states[from].transitions;
        let at = transitions
            .binary_search_by_key(&symbol, |&(s, _)| s)
            .expect("the path of a rule follows transitions");
        transitions[at].1
    };

    // The tokens each goto target shifts directly, and the reads relation over nullable gotos.
    let mut sets: Vec<Tokens> = Vec::with_capacity(gotos.len());
    let mut reads: Vec<Vec<usize>> = Vec::with_capacity(gotos.len());
    for &(_, _, to) in &gotos {
        let mut set = Tokens::new(tokens);
        let mut edges = Vec::new();
        for &(symbol, _) in &states[to].transitions {
            if symbol < tokens {
                set.insert(symbol);
            } else if nullable[symbol - tokens] {
                edges.push(index[&(to, symbol)]);
            }
        }
        sets.push(set);
        reads.push(edges);
    }
    digraph(&reads, &mut sets);

    // The includes relation and the lookback of each reduction.
    let mut includes: Vec<Vec<usize>> = vec![Vec::new(); gotos.len()];
    let mut lookback: HashMap<(usize, usize), Vec<usize>> = HashMap::new();
    let mut path = Vec::new();
    for (g, &(symbol, from, _)) in gotos.iter().enumerate() {
        for &rule in &by_lhs[symbol - tokens] {
            path.clear();
            path.push(from);
            let mut state = from;
            for &s in &rules[rule].rhs {
                state = target(state, s);
                path.push(state);
            }
            if states[state].lookaheads.is_some() {
                lookback.entry((state, rule)).or_default().push(g);
            }
            for (at, &s) in rules[rule].rhs.iter().enumerate().rev() {
                if s < tokens {
                    break;
                }
                includes[index[&(path[at], s)]].push(g);
                if !nullable[s - tokens] {
                    break;
                }
            }
        }
    }
    digraph(&includes, &mut sets);

    for (number, state) in states.iter_mut().enumerate() {
        let Some(lookaheads) = &mut state.lookaheads else { continue };
        for (at, &rule) in state.reductions.iter().enumerate() {
            for &g in lookback.get(&(number, rule)).map_or(&[][..], Vec::as_slice) {
                lookaheads[at].union(&sets[g]);
            }
        }
    }
}

/// The digraph algorithm: makes each set the union of the sets that its node reaches.
fn digraph(edges: &[Vec<usize>], sets: &mut [Tokens]) {
    let nodes = edges.len();
    let mut depth = vec![0usize; nodes];
    let mut stack = Vec::new();
    for node in 0..nodes {
        if depth[node] == 0 {
            traverse(node, edges, sets, &mut depth, &mut stack);
        }
    }
}

fn traverse(
    node: usize,
    edges: &[Vec<usize>],
    sets: &mut [Tokens],
    depth: &mut [usize],
    stack: &mut Vec<usize>,
) {
    // The recursive form with an explicit stack of frames, so that a long chain of edges does not use the call stack. A frame is the node, its next edge and its depth on `stack`.
    let mut frames = vec![(node, 0usize, 0usize)];
    stack.push(node);
    depth[node] = stack.len();
    frames[0].2 = stack.len();
    while let Some(frame) = frames.last_mut() {
        let (x, next, own) = *frame;
        if let Some(&y) = edges[x].get(next) {
            frame.1 += 1;
            if depth[y] == 0 {
                stack.push(y);
                depth[y] = stack.len();
                frames.push((y, 0, stack.len()));
                continue;
            }
            fold(x, y, sets, depth);
            continue;
        }
        frames.pop();
        if depth[x] == own {
            loop {
                let top = stack.pop().expect("the node is on the stack");
                depth[top] = usize::MAX;
                if top == x {
                    break;
                }
                sets[top] = sets[x].clone();
            }
        }
        if let Some(&(parent, _, _)) = frames.last() {
            fold(parent, x, sets, depth);
        }
    }
}

/// The step after an edge from `x` to `y`: `x` takes the lower depth and the set of `y`.
fn fold(x: usize, y: usize, sets: &mut [Tokens], depth: &mut [usize]) {
    if depth[y] < depth[x] {
        depth[x] = depth[y];
    }
    if x != y {
        let (a, b) = if x < y {
            let (low, high) = sets.split_at_mut(y);
            (&mut low[x], &high[0])
        } else {
            let (low, high) = sets.split_at_mut(x);
            (&mut high[0], &low[y])
        };
        a.union(b);
    }
}

/// Resolves the conflicts with precedence and makes the action row and the default reduction of each state, as bison's `conflicts.c` and `tables.c` do.
fn actions(
    grammar: &Grammar,
    states: &mut [State],
    expect: usize,
) -> Result<(Vec<Row>, Vec<u16>), String> {
    let tokens = grammar.tokens;
    let rule_prec = |rule: usize| -> u16 {
        grammar.rules[rule].prec.map_or(0, |token| grammar.precedence[token].0)
    };
    let mut conflicts = Vec::new();
    let mut rows = Vec::with_capacity(states.len());
    let mut defaults = Vec::with_capacity(states.len());
    for (number, state) in states.iter_mut().enumerate() {
        let mut shifts = Tokens::new(tokens);
        for &(symbol, _) in &state.transitions {
            if symbol < tokens {
                shifts.insert(symbol);
            }
        }
        let mut disabled = Tokens::new(tokens);
        let mut errors = Vec::new();
        if let Some(lookaheads) = &mut state.lookaheads {
            for (at, &rule) in state.reductions.iter().enumerate() {
                let level = rule_prec(rule);
                if level == 0 || lookaheads[at].disjoint(&shifts) {
                    continue;
                }
                for token in 0..tokens {
                    if !(lookaheads[at].contains(token) && shifts.contains(token)) {
                        continue;
                    }
                    let (token_level, assoc) = grammar.precedence[token];
                    if token_level == 0 {
                        continue;
                    }
                    let reduce =
                        token_level < level || (token_level == level && assoc != Assoc::Right);
                    let shift =
                        token_level > level || (token_level == level && assoc != Assoc::Left);
                    if reduce {
                        shifts.remove(token);
                        disabled.insert(token);
                    }
                    if shift {
                        lookaheads[at].remove(token);
                    }
                    if reduce && shift {
                        errors.push(token);
                    }
                }
            }
            let mut seen = shifts.clone();
            for (at, lookahead) in lookaheads.iter().enumerate() {
                for token in lookahead.iter() {
                    if seen.contains(token) {
                        let rule = state.reductions[at];
                        conflicts.push(format!(
                            "state {number}: {} and rule {} ({})",
                            grammar.symbols[token], rule, grammar.rules[rule].name
                        ));
                    }
                }
                seen.union(lookahead);
            }
        }

        // The row: the reductions in reverse order so that the first rule wins, then the shifts, then the errors of `%nonassoc`.
        let mut row = vec![0i32; tokens];
        if let Some(lookaheads) = &state.lookaheads {
            for (at, lookahead) in lookaheads.iter().enumerate().rev() {
                for token in lookahead.iter() {
                    row[token] = -(state.reductions[at] as i32);
                }
            }
        }
        for &(symbol, to) in &state.transitions {
            if symbol < tokens && !disabled.contains(symbol) {
                row[symbol] = to as i32;
            }
        }
        for &token in &errors {
            row[token] = ERROR;
        }
        let mut default = 0usize;
        if !state.reductions.is_empty() {
            if state.lookaheads.is_none() {
                default = state.reductions[0];
            } else {
                let mut most = 0;
                for &rule in &state.reductions {
                    let count = row.iter().filter(|&&a| a == -(rule as i32)).count();
                    if count > most {
                        most = count;
                        default = rule;
                    }
                }
                if most > 0 {
                    for action in &mut row {
                        if *action == -(default as i32) {
                            *action = 0;
                        }
                    }
                }
            }
        }
        if default == 0 {
            for action in &mut row {
                if *action == ERROR {
                    *action = 0;
                }
            }
        }
        rows.push(row.iter().enumerate().filter(|&(_, &a)| a != 0).map(|(t, &a)| (t, a)).collect());
        defaults.push(default as u16);
    }
    if conflicts.len() != expect {
        let mut message = format!(
            "the grammar has {} conflicts that precedence does not resolve and %expect is {expect}",
            conflicts.len()
        );
        for conflict in conflicts.iter().take(20) {
            message.push_str("\n  ");
            message.push_str(conflict);
        }
        return Err(message);
    }
    Ok((rows, defaults))
}

/// Packs the action rows and the goto columns into one table, each at its own base, as bison's `pack_vector` does. Two rows never share a base, so that the check of an entry tells which row it belongs to. Two action rows that are the same share one base.
fn pack(
    actions: &[Vec<(usize, i32)>],
    gotos: &[Vec<(usize, i32)>],
    states: usize,
) -> (Vec<Option<i32>>, Vec<i32>, Vec<i32>) {
    let vectors: Vec<&Vec<(usize, i32)>> = actions.iter().chain(gotos).collect();
    let mut order: Vec<usize> = (0..vectors.len()).filter(|&v| !vectors[v].is_empty()).collect();
    let width =
        |v: usize| vectors[v].last().map_or(0, |e| e.0) - vectors[v].first().map_or(0, |e| e.0);
    order.sort_by(|&a, &b| {
        width(b).cmp(&width(a)).then(vectors[b].len().cmp(&vectors[a].len())).then(a.cmp(&b))
    });

    let mut bases: Vec<Option<i32>> = vec![None; vectors.len()];
    let mut table: Vec<i32> = Vec::new();
    let mut check: Vec<i32> = Vec::new();
    let mut used_bases = std::collections::HashSet::new();
    let mut same: HashMap<&[(usize, i32)], i32> = HashMap::new();
    let mut lowest_free = 0usize;
    for &v in &order {
        let entries = vectors[v].as_slice();
        if v < states
            && let Some(&base) = same.get(entries)
        {
            bases[v] = Some(base);
            continue;
        }
        let first = entries[0].0 as i32;
        let mut base = lowest_free as i32 - first;
        loop {
            let fits = !used_bases.contains(&base)
                && entries.iter().all(|&(at, _)| {
                    let at = (base + at as i32) as usize;
                    at >= check.len() || check[at] == -1
                });
            if fits {
                break;
            }
            base += 1;
        }
        for &(at, value) in entries {
            let at = (base + at as i32) as usize;
            if at >= check.len() {
                check.resize(at + 1, -1);
                table.resize(at + 1, 0);
            }
            check[at] = at as i32 - base;
            table[at] = value;
        }
        while lowest_free < check.len() && check[lowest_free] != -1 {
            lowest_free += 1;
        }
        used_bases.insert(base);
        bases[v] = Some(base);
        if v < states {
            same.insert(entries, base);
        }
    }
    (bases, table, check)
}
