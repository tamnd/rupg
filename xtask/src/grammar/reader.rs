//! Reads a bison grammar file.
//!
//! The reader keeps what the parser tables need: the terminals, the precedence declarations, the rules, `%prec`, `%start`, `%expect` and `%expect-rr`. It skips the C code in the prologue, in the actions and in the braced arguments of the other directives. A mid-rule action becomes a new nonterminal `$@N` with one empty rule, as in bison.

use std::collections::HashMap;

/// The associativity of a precedence level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Assoc {
    Left,
    Right,
    Nonassoc,
    /// `%precedence`: a level with no associativity. A conflict at the same level stays a conflict.
    Precedence,
}

/// A symbol. Terminals are `0..terminals.len()`, nonterminals follow.
pub(crate) type Sym = u32;

#[derive(Debug)]
pub(crate) struct Rule {
    pub(crate) lhs: Sym,
    pub(crate) rhs: Vec<Sym>,
    /// The terminal that gives the rule its precedence: the `%prec` symbol, else the last terminal of the right side.
    pub(crate) prec_sym: Option<Sym>,
    /// The line of the rule in the grammar file.
    pub(crate) line: usize,
}

#[derive(Debug)]
pub(crate) struct Grammar {
    /// The names of all symbols. Terminal 0 is `$end`, 1 is `error` and 2 is `$undefined`. The first nonterminal is `$accept`.
    pub(crate) names: Vec<String>,
    pub(crate) terminals: usize,
    /// The precedence level and associativity of each terminal. Level 0 means no precedence.
    pub(crate) prec: Vec<(u32, Assoc)>,
    /// Rule 0 is `$accept: start $end`.
    pub(crate) rules: Vec<Rule>,
    pub(crate) expect: Option<usize>,
    pub(crate) expect_rr: Option<usize>,
}

impl Grammar {
    pub(crate) fn name(&self, sym: Sym) -> &str {
        &self.names[sym as usize]
    }

    /// The precedence of a rule: the level and associativity of its precedence terminal, or level 0.
    pub(crate) fn rule_prec(&self, rule: usize) -> u32 {
        self.rules[rule].prec_sym.map_or(0, |s| self.prec[s as usize].0)
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    /// A character literal with its quotes, as bison prints it: `'('`.
    Char(String),
    Str(String),
    Directive(String),
    Tag,
    Number(usize),
    Colon,
    Pipe,
    Semi,
    Action,
    Sections,
    Other,
}

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    line: usize,
}

impl Lexer<'_> {
    fn err(&self, message: &str) -> String {
        format!("line {}: {message}", self.line)
    }

    fn peek(&self, ahead: usize) -> u8 {
        self.src.get(self.pos + ahead).copied().unwrap_or(0)
    }

    fn bump(&mut self) -> u8 {
        let c = self.peek(0);
        self.pos += 1;
        if c == b'\n' {
            self.line += 1;
        }
        c
    }

    fn at_end(&self) -> bool {
        self.pos >= self.src.len()
    }

    /// Skips white space and comments.
    fn skip_space(&mut self) -> Result<(), String> {
        loop {
            match (self.peek(0), self.peek(1)) {
                (b' ' | b'\t' | b'\r' | b'\n' | b'\x0c', _) => {
                    self.bump();
                }
                (b'/', b'*') => self.skip_block_comment()?,
                (b'/', b'/') => {
                    while !self.at_end() && self.peek(0) != b'\n' {
                        self.bump();
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn skip_block_comment(&mut self) -> Result<(), String> {
        let start = self.line;
        self.pos += 2;
        while !(self.peek(0) == b'*' && self.peek(1) == b'/') {
            if self.at_end() {
                return Err(format!("line {start}: the comment does not end"));
            }
            self.bump();
        }
        self.pos += 2;
        Ok(())
    }

    /// Skips a quoted C string or character literal. The opening quote is at `pos`.
    fn skip_quoted(&mut self) -> Result<(), String> {
        let quote = self.bump();
        loop {
            match self.bump() {
                0 if self.at_end() => return Err(self.err("a quoted literal does not end")),
                b'\\' => {
                    self.bump();
                }
                b'\n' => return Err(self.err("a quoted literal contains a new line")),
                c if c == quote => return Ok(()),
                _ => {}
            }
        }
    }

    /// Skips a braced block of C code. The opening brace is at `pos`.
    fn skip_braces(&mut self) -> Result<(), String> {
        let start = self.line;
        let mut depth = 0usize;
        loop {
            if self.at_end() {
                return Err(format!("line {start}: the braced code does not end"));
            }
            match (self.peek(0), self.peek(1)) {
                (b'{', _) => {
                    depth += 1;
                    self.bump();
                }
                (b'}', _) => {
                    depth -= 1;
                    self.bump();
                    if depth == 0 {
                        return Ok(());
                    }
                }
                (b'"' | b'\'', _) => self.skip_quoted()?,
                (b'/', b'*') => self.skip_block_comment()?,
                (b'/', b'/') => {
                    while !self.at_end() && self.peek(0) != b'\n' {
                        self.bump();
                    }
                }
                _ => {
                    self.bump();
                }
            }
        }
    }

    fn next(&mut self) -> Result<Option<(Tok, usize)>, String> {
        self.skip_space()?;
        if self.at_end() {
            return Ok(None);
        }
        let line = self.line;
        let c = self.peek(0);
        let tok = match c {
            b'%' if self.peek(1) == b'%' => {
                self.pos += 2;
                Tok::Sections
            }
            b'%' if self.peek(1) == b'{' => {
                self.pos += 2;
                while !(self.peek(0) == b'%' && self.peek(1) == b'}') {
                    if self.at_end() {
                        return Err(format!("line {line}: the prologue does not end"));
                    }
                    self.bump();
                }
                self.pos += 2;
                Tok::Other
            }
            b'%' => {
                self.pos += 1;
                let start = self.pos;
                while self.peek(0).is_ascii_alphanumeric() || matches!(self.peek(0), b'-' | b'_') {
                    self.pos += 1;
                }
                Tok::Directive(String::from_utf8_lossy(&self.src[start..self.pos]).into_owned())
            }
            b'{' => {
                self.skip_braces()?;
                Tok::Action
            }
            b'<' => {
                while self.peek(0) != b'>' {
                    if self.at_end() || self.peek(0) == b'\n' {
                        return Err(format!("line {line}: the type tag does not end"));
                    }
                    self.bump();
                }
                self.bump();
                Tok::Tag
            }
            b'\'' => {
                let start = self.pos;
                self.skip_quoted()?;
                Tok::Char(String::from_utf8_lossy(&self.src[start..self.pos]).into_owned())
            }
            b'"' => {
                let start = self.pos;
                self.skip_quoted()?;
                Tok::Str(String::from_utf8_lossy(&self.src[start..self.pos]).into_owned())
            }
            b':' => {
                self.bump();
                Tok::Colon
            }
            b'|' => {
                self.bump();
                Tok::Pipe
            }
            b';' => {
                self.bump();
                Tok::Semi
            }
            b'[' => {
                // A named reference such as `expr[left]`. The name is for the actions only.
                while self.peek(0) != b']' {
                    if self.at_end() {
                        return Err(format!("line {line}: the named reference does not end"));
                    }
                    self.bump();
                }
                self.bump();
                return self.next();
            }
            c if c.is_ascii_digit() => {
                let start = self.pos;
                while self.peek(0).is_ascii_digit() {
                    self.pos += 1;
                }
                let text =
                    std::str::from_utf8(&self.src[start..self.pos]).expect("digits are ASCII");
                Tok::Number(text.parse().map_err(|_| self.err("the number is too large"))?)
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c == b'.' => {
                let start = self.pos;
                while self.peek(0).is_ascii_alphanumeric() || matches!(self.peek(0), b'_' | b'.') {
                    self.pos += 1;
                }
                Tok::Ident(String::from_utf8_lossy(&self.src[start..self.pos]).into_owned())
            }
            _ => {
                self.bump();
                Tok::Other
            }
        };
        Ok(Some((tok, line)))
    }
}

/// One element of the right side of a rule, before the symbols get numbers.
enum Element {
    Symbol(String),
    Action,
}

struct RawRule {
    lhs: String,
    elements: Vec<Element>,
    prec: Option<String>,
    line: usize,
}

pub(crate) fn read(text: &str) -> Result<Grammar, String> {
    let mut lexer = Lexer { src: text.as_bytes(), pos: 0, line: 1 };
    let mut tokens = Vec::new();
    let mut sections = 0;
    while let Some((tok, line)) = lexer.next()? {
        if tok == Tok::Sections {
            sections += 1;
            if sections == 2 {
                break;
            }
        }
        tokens.push((tok, line));
    }
    let split =
        tokens.iter().position(|(t, _)| *t == Tok::Sections).ok_or("the grammar has no %% line")?;
    let (decls, rules) = tokens.split_at(split);
    let rules = &rules[1..];

    let mut terminals: Vec<String> = vec!["$end".into(), "error".into(), "$undefined".into()];
    let mut prec_of: HashMap<String, (u32, Assoc)> = HashMap::new();
    let mut aliases: HashMap<String, String> = HashMap::new();
    let mut start = None;
    let mut expect = None;
    let mut expect_rr = None;
    let mut level = 0u32;

    let mut i = 0;
    while i < decls.len() {
        let (tok, line) = &decls[i];
        i += 1;
        let Tok::Directive(name) = tok else {
            continue;
        };
        // The arguments of a directive run to the next directive.
        let end = decls[i..]
            .iter()
            .position(|(t, _)| matches!(t, Tok::Directive(_)))
            .map_or(decls.len(), |p| i + p);
        let args = &decls[i..end];
        let at = |message: String| format!("line {line}: {message}");
        match name.as_str() {
            "token" | "left" | "right" | "nonassoc" | "precedence" => {
                let assoc = match name.as_str() {
                    "left" => Some(Assoc::Left),
                    "right" => Some(Assoc::Right),
                    "nonassoc" => Some(Assoc::Nonassoc),
                    "precedence" => Some(Assoc::Precedence),
                    _ => None,
                };
                if assoc.is_some() {
                    level += 1;
                }
                let mut last: Option<String> = None;
                for (arg, _) in args {
                    match arg {
                        Tok::Ident(s) | Tok::Char(s) => {
                            if !terminals.contains(s) {
                                terminals.push(s.clone());
                            }
                            if let Some(assoc) = assoc
                                && prec_of.insert(s.clone(), (level, assoc)).is_some()
                            {
                                return Err(at(format!("{s} has two precedence declarations")));
                            }
                            last = Some(s.clone());
                        }
                        Tok::Str(alias) => {
                            let Some(token) = &last else {
                                return Err(at(format!("the alias {alias} has no token")));
                            };
                            aliases.insert(alias.clone(), token.clone());
                        }
                        Tok::Tag | Tok::Number(_) => {}
                        other => return Err(at(format!("unexpected {other:?} in %{name}"))),
                    }
                }
            }
            "start" => match args.first() {
                Some((Tok::Ident(s), _)) => start = Some(s.clone()),
                _ => return Err(at("%start needs a symbol".to_string())),
            },
            "expect" | "expect-rr" => match args.first() {
                Some((Tok::Number(n), _)) => {
                    if name == "expect" {
                        expect = Some(*n);
                    } else {
                        expect_rr = Some(*n);
                    }
                }
                _ => return Err(at(format!("%{name} needs a number"))),
            },
            "glr-parser" => return Err(at("a GLR grammar is not supported".to_string())),
            "no-default-prec" => return Err(at("%no-default-prec is not supported".to_string())),
            _ => {}
        }
        i = end;
    }

    let raw = read_rules(rules)?;
    if raw.is_empty() {
        return Err("the grammar has no rules".to_string());
    }

    // Number the nonterminals in the order of their first rule. The mid-rule actions get their names here.
    let mut nonterminals: Vec<String> = vec!["$accept".into()];
    let mut expanded: Vec<(String, Vec<String>, Option<String>, usize)> = Vec::new();
    let mut midrule = 0;
    for rule in &raw {
        let mut rhs = Vec::new();
        for (index, element) in rule.elements.iter().enumerate() {
            match element {
                Element::Symbol(name) => {
                    rhs.push(aliases.get(name).cloned().unwrap_or_else(|| name.clone()))
                }
                // An action that is not the last element is a mid-rule action.
                Element::Action if index + 1 < rule.elements.len() => {
                    midrule += 1;
                    let name = format!("$@{midrule}");
                    expanded.push((name.clone(), Vec::new(), None, rule.line));
                    rhs.push(name);
                }
                Element::Action => {}
            }
        }
        expanded.push((rule.lhs.clone(), rhs, rule.prec.clone(), rule.line));
    }
    for (lhs, ..) in &expanded {
        if terminals.contains(lhs) {
            return Err(format!("{lhs} is a token and also has rules"));
        }
        if !nonterminals.contains(lhs) {
            nonterminals.push(lhs.clone());
        }
    }
    // A character literal in a rule is a terminal even if no declaration names it.
    for (_, rhs, ..) in &expanded {
        for s in rhs {
            if s.starts_with('\'') && !terminals.contains(s) {
                terminals.push(s.clone());
            }
        }
    }

    let nterms = terminals.len();
    let mut names = terminals;
    names.extend(nonterminals);
    let index: HashMap<&str, Sym> =
        names.iter().enumerate().map(|(i, n)| (n.as_str(), i as Sym)).collect();
    let lookup = |name: &str, line: usize| {
        index
            .get(name)
            .copied()
            .ok_or_else(|| format!("line {line}: {name} is not a token and has no rules"))
    };

    let start_name = start.unwrap_or_else(|| raw[0].lhs.clone());
    let start_sym = lookup(&start_name, 0)?;
    if (start_sym as usize) < nterms {
        return Err(format!("the start symbol {start_name} is a token"));
    }
    let accept = nterms as Sym;
    let mut out_rules =
        vec![Rule { lhs: accept, rhs: vec![start_sym, 0], prec_sym: None, line: 0 }];
    for (lhs, rhs, prec, line) in &expanded {
        let rhs: Vec<Sym> = rhs.iter().map(|s| lookup(s, *line)).collect::<Result<_, _>>()?;
        let prec_sym = match prec {
            Some(p) => {
                let s = lookup(aliases.get(p).map_or(p.as_str(), String::as_str), *line)?;
                if s as usize >= nterms {
                    return Err(format!("line {line}: %prec {p} is not a token"));
                }
                Some(s)
            }
            None => rhs.iter().rev().copied().find(|&s| (s as usize) < nterms),
        };
        out_rules.push(Rule { lhs: lookup(lhs, *line)?, rhs, prec_sym, line: *line });
    }

    let prec = names[..nterms]
        .iter()
        .map(|n| prec_of.get(n).copied().unwrap_or((0, Assoc::Precedence)))
        .collect();
    Ok(Grammar { names, terminals: nterms, prec, rules: out_rules, expect, expect_rr })
}

fn read_rules(tokens: &[(Tok, usize)]) -> Result<Vec<RawRule>, String> {
    let mut rules = Vec::new();
    let mut lhs: Option<String> = None;
    let mut current: Option<RawRule> = None;
    let mut i = 0;
    while i < tokens.len() {
        let (tok, line) = &tokens[i];
        let at = |message: &str| format!("line {line}: {message}");
        match tok {
            Tok::Ident(name) if matches!(tokens.get(i + 1), Some((Tok::Colon, _))) => {
                rules.extend(current.take());
                lhs = Some(name.clone());
                current = Some(RawRule {
                    lhs: name.clone(),
                    elements: Vec::new(),
                    prec: None,
                    line: *line,
                });
                i += 2;
                continue;
            }
            Tok::Ident(name) | Tok::Char(name) | Tok::Str(name) => {
                let rule = current.as_mut().ok_or_else(|| at("a symbol outside a rule"))?;
                rule.elements.push(Element::Symbol(name.clone()));
            }
            Tok::Action => {
                let rule = current.as_mut().ok_or_else(|| at("an action outside a rule"))?;
                rule.elements.push(Element::Action);
            }
            Tok::Pipe => {
                let name = lhs.clone().ok_or_else(|| at("| outside a rule"))?;
                rules.extend(current.take());
                current =
                    Some(RawRule { lhs: name, elements: Vec::new(), prec: None, line: *line });
            }
            Tok::Semi => {
                rules.extend(current.take());
            }
            Tok::Directive(d) if d == "prec" => {
                let rule = current.as_mut().ok_or_else(|| at("%prec outside a rule"))?;
                match tokens.get(i + 1) {
                    Some((Tok::Ident(s) | Tok::Char(s) | Tok::Str(s), _)) => {
                        rule.prec = Some(s.clone())
                    }
                    _ => return Err(at("%prec needs a token")),
                }
                i += 2;
                continue;
            }
            Tok::Directive(d) if d == "empty" => {
                current.as_ref().ok_or_else(|| at("%empty outside a rule"))?;
            }
            Tok::Directive(d) => return Err(at(&format!("%{d} is not supported in the rules"))),
            other => return Err(at(&format!("unexpected {other:?} in the rules"))),
        }
        i += 1;
    }
    rules.extend(current);
    Ok(rules)
}

#[cfg(test)]
mod tests {
    use super::{Assoc, read};

    #[test]
    fn reads_rules_precedence_and_midrule_actions() {
        let g = read(
            "%{\n#include <x.h>\n%}\n%union { int i; }\n%token <i> NUM\n%left '+' '-'\n%right UMINUS\n%expect 2\n%%\n\
             e: e '+' e { $$ = $1 + $3; }\n | '-' e %prec UMINUS\n | NUM { a(); } NUM { b(\"}\"); }\n | /* empty */ ;\n%%\nint main() {}\n",
        )
        .unwrap();
        assert_eq!(g.expect, Some(2));
        let names: Vec<_> = g.rules.iter().map(|r| g.name(r.lhs).to_string()).collect();
        assert_eq!(names, ["$accept", "e", "e", "$@1", "e", "e"]);
        assert_eq!(g.rules[3].rhs.len(), 0);
        assert_eq!(g.rules[4].rhs.len(), 3);
        let uminus = g.names.iter().position(|n| n == "UMINUS").unwrap() as u32;
        assert_eq!(g.rules[2].prec_sym, Some(uminus));
        assert_eq!(g.prec[uminus as usize], (2, Assoc::Right));
        let plus = g.names.iter().position(|n| n == "'+'").unwrap();
        assert_eq!(g.prec[plus], (1, Assoc::Left));
        assert_eq!(g.rule_prec(1), 1);
        assert_eq!(g.rule_prec(5), 0);
    }

    #[test]
    fn rejects_an_undefined_symbol() {
        let err = read("%token A\n%%\ns: A B ;\n").unwrap_err();
        assert!(err.contains("B is not a token"), "{err}");
    }

    #[test]
    fn a_rule_may_omit_the_semicolon() {
        let g = read("%token A\n%%\ns: t\nt: A\n").unwrap();
        assert_eq!(g.rules.len(), 3);
    }
}
