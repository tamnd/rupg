//! The reader of the PostgreSQL grammar, `gram.y`, and the two files written from it without the C.
//!
//! The reader keeps what the table generator needs: the tokens, the precedence declarations, every rule, every alternative and every `%prec`. It also keeps what the generator of the action glue needs: the prologue, the `%union`, the tag of each symbol and the C action of each alternative. `gram.y` has no mid-rule action, and the reader refuses one, because an action between two symbols is a hidden empty rule and the generator does not make those.
//!
//! The symbols are numbered as bison 2.3 numbers them, so a state number of the generated tables is the state number in `bison -v` output for the same file. `$end`, `error` and `$undefined` are tokens 0, 1 and 2. The other tokens follow in the order the file first names them as a token, which is a `%token` line, a precedence line or a character literal. `$accept` is the first nonterminal and the others follow in the order the file first defines them.
//!
//! The plan is `spec/07-sql-types-and-catalog.md` section 7.2.
//!
//! Lifted from `xtask/src/postgres/gram.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use std::collections::HashMap;
use std::fmt::Write as _;

/// How a precedence level groups.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Assoc {
    Left,
    Right,
    Nonassoc,
}

/// One rule. Rule 0 is `$accept: <start> $end`, as in bison.
pub(super) struct Rule {
    pub(super) lhs: usize,
    pub(super) rhs: Vec<usize>,
    /// The token that gives the rule its precedence: the `%prec` token, or else the last token on the right side. The rule has no precedence when this token has none.
    pub(super) prec: Option<usize>,
    /// The name of the alternative, the rule name and the number of the alternative from 1.
    pub(super) name: String,
    /// The C action in its braces, or `None` when the alternative has no action.
    pub(super) action: Option<String>,
    /// The line of `gram.y` where the alternative starts: the line of its first item.
    pub(super) line: usize,
}

/// One declaration line, kept so that `gram.rules` declares the tokens in the same order.
struct Declaration {
    directive: &'static str,
    symbols: Vec<usize>,
}

/// The grammar of `gram.y` without the C.
pub(super) struct Grammar {
    /// Every symbol, the tokens first. A character literal keeps its quotes, for example `'+'`.
    pub(super) symbols: Vec<String>,
    /// The number of tokens. A symbol is a token when its number is below this.
    pub(super) tokens: usize,
    /// The precedence level of each token from 1 up, 0 for none, and its grouping.
    pub(super) precedence: Vec<(u16, Assoc)>,
    pub(super) rules: Vec<Rule>,
    /// The number that `%expect` gives.
    pub(super) expect: usize,
    /// The `%union` member that `%token <tag>` or `%type <tag>` gives each symbol, or `None`.
    pub(super) tags: Vec<Option<String>>,
    /// The C in `%{ ... %}`, without the two lines that open and close it.
    pub(super) prologue: String,
    /// The body of `%union`, in its braces.
    pub(super) union: String,
    declarations: Vec<Declaration>,
}

/// One lexical item of the grammar file.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Item {
    /// A name or a number.
    Word(String),
    /// A character literal, with its quotes.
    Char(String),
    /// A `%` directive, without the `%`.
    Directive(String),
    /// A `<tag>`, without the angle brackets.
    Tag(String),
    /// A string in double quotes.
    Text,
    /// A block in braces, with the braces: an action, the `%union` body or a `%parse-param` argument.
    Braces(String),
    Colon,
    Pipe,
    Semicolon,
    Equals,
    /// The `%%` that ends the rules.
    End,
}

/// Splits the text of the grammar file into items. `from` is the byte offset where the split starts, `line` the line number there.
struct Scanner<'a> {
    text: &'a [u8],
    at: usize,
    line: usize,
}

impl Scanner<'_> {
    fn error(&self, message: &str) -> String {
        format!("gram.y line {}: {message}", self.line)
    }

    fn peek(&self, ahead: usize) -> u8 {
        self.text.get(self.at + ahead).copied().unwrap_or(0)
    }

    fn bump(&mut self) {
        if self.peek(0) == b'\n' {
            self.line += 1;
        }
        self.at += 1;
    }

    /// Skips a C comment that starts at the cursor.
    fn comment(&mut self) -> Result<(), String> {
        if self.peek(1) == b'/' {
            while self.at < self.text.len() && self.peek(0) != b'\n' {
                self.bump();
            }
            return Ok(());
        }
        let start = self.line;
        self.at += 2;
        while !(self.peek(0) == b'*' && self.peek(1) == b'/') {
            if self.at >= self.text.len() {
                return Err(format!("gram.y line {start}: a comment does not end"));
            }
            self.bump();
        }
        self.at += 2;
        Ok(())
    }

    /// Skips a C string or character literal that starts at the cursor.
    fn quoted(&mut self) -> Result<(), String> {
        let quote = self.peek(0);
        let start = self.line;
        self.bump();
        loop {
            match self.peek(0) {
                0 | b'\n' => return Err(format!("gram.y line {start}: a literal does not end")),
                b'\\' => {
                    self.bump();
                    self.bump();
                }
                c if c == quote => {
                    self.bump();
                    return Ok(());
                }
                _ => self.bump(),
            }
        }
    }

    /// Skips a block in braces that starts at the cursor, with the strings, characters and comments of the C in it.
    fn braces(&mut self) -> Result<(), String> {
        let start = self.line;
        let mut depth = 0usize;
        loop {
            match self.peek(0) {
                0 => return Err(format!("gram.y line {start}: a block in braces does not end")),
                b'{' => {
                    depth += 1;
                    self.bump();
                }
                b'}' => {
                    depth -= 1;
                    self.bump();
                    if depth == 0 {
                        return Ok(());
                    }
                }
                b'"' | b'\'' => self.quoted()?,
                b'/' if matches!(self.peek(1), b'*' | b'/') => self.comment()?,
                _ => self.bump(),
            }
        }
    }

    /// The next item and its line, or `None` at the end of the text.
    fn next(&mut self) -> Result<Option<(Item, usize)>, String> {
        loop {
            match self.peek(0) {
                0 => return Ok(None),
                b' ' | b'\t' | b'\r' | b'\n' => self.bump(),
                b'/' if matches!(self.peek(1), b'*' | b'/') => self.comment()?,
                _ => break,
            }
        }
        let line = self.line;
        let start = self.at;
        let item = match self.peek(0) {
            b':' => {
                self.bump();
                Item::Colon
            }
            b'|' => {
                self.bump();
                Item::Pipe
            }
            b';' => {
                self.bump();
                Item::Semicolon
            }
            b'=' => {
                self.bump();
                Item::Equals
            }
            b'{' => {
                self.braces()?;
                Item::Braces(self.slice(start))
            }
            b'"' => {
                self.quoted()?;
                Item::Text
            }
            b'\'' => {
                self.quoted()?;
                Item::Char(self.slice(start))
            }
            b'<' => {
                while self.peek(0) != b'>' {
                    if self.peek(0) == 0 || self.peek(0) == b'\n' {
                        return Err(self.error("a tag does not end"));
                    }
                    self.bump();
                }
                self.bump();
                Item::Tag(String::from_utf8_lossy(&self.text[start + 1..self.at - 1]).into_owned())
            }
            b'%' if self.peek(1) == b'%' => {
                self.at += 2;
                Item::End
            }
            b'%' => {
                self.bump();
                while matches!(self.peek(0), b'a'..=b'z' | b'-' | b'_') {
                    self.bump();
                }
                Item::Directive(self.slice(start + 1))
            }
            c if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' => {
                while self.peek(0).is_ascii_alphanumeric() || matches!(self.peek(0), b'_' | b'.') {
                    self.bump();
                }
                Item::Word(self.slice(start))
            }
            c => return Err(self.error(&format!("unexpected character {:?}", c as char))),
        };
        Ok(Some((item, line)))
    }

    fn slice(&self, start: usize) -> String {
        String::from_utf8_lossy(&self.text[start..self.at]).into_owned()
    }
}

/// Collects the symbols and gives them their bison numbers at the end.
struct Symbols {
    names: Vec<String>,
    index: HashMap<String, usize>,
    /// The order in which each symbol became a token, or a nonterminal.
    token_order: Vec<usize>,
    nonterminal_order: Vec<usize>,
    kind: Vec<Kind>,
    precedence: Vec<(u16, Assoc)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Unknown,
    Token,
    Nonterminal,
}

impl Symbols {
    fn get(&mut self, name: &str) -> usize {
        if let Some(&symbol) = self.index.get(name) {
            return symbol;
        }
        let symbol = self.names.len();
        self.names.push(name.to_string());
        self.index.insert(name.to_string(), symbol);
        self.kind.push(Kind::Unknown);
        self.precedence.push((0, Assoc::Left));
        symbol
    }

    fn token(&mut self, name: &str) -> Result<usize, String> {
        let symbol = self.get(name);
        match self.kind[symbol] {
            Kind::Token => {}
            Kind::Unknown => {
                self.kind[symbol] = Kind::Token;
                self.token_order.push(symbol);
            }
            Kind::Nonterminal => return Err(format!("{name} is a rule and is declared a token")),
        }
        Ok(symbol)
    }

    fn nonterminal(&mut self, name: &str) -> Result<usize, String> {
        let symbol = self.get(name);
        match self.kind[symbol] {
            Kind::Nonterminal => {}
            Kind::Unknown => {
                self.kind[symbol] = Kind::Nonterminal;
                self.nonterminal_order.push(symbol);
            }
            Kind::Token => return Err(format!("{name} is a token and has a rule")),
        }
        Ok(symbol)
    }
}

/// One alternative before the symbols get their final numbers.
struct Alternative {
    lhs: usize,
    rhs: Vec<usize>,
    prec: Option<usize>,
    name: String,
    action: Option<String>,
    line: usize,
}

/// Reads `gram.y`.
pub(super) fn read(text: &str) -> Result<Grammar, String> {
    // The prologue in `%{ ... %}` is C and goes before anything the scanner reads.
    let open = text.find("%{").ok_or("gram.y has no prologue")?;
    let close = text[open..].find("\n%}").ok_or("the prologue of gram.y does not end")? + open + 3;
    let prologue = text[open + 2..close - 2].to_string();
    let line = text[..close].matches('\n').count() + 1;
    let mut scanner = Scanner { text: text.as_bytes(), at: close, line };

    let mut symbols = Symbols {
        names: Vec::new(),
        index: HashMap::new(),
        token_order: Vec::new(),
        nonterminal_order: Vec::new(),
        kind: Vec::new(),
        precedence: Vec::new(),
    };
    for name in ["$end", "error", "$undefined"] {
        symbols.token(name)?;
    }
    let accept = symbols.nonterminal("$accept")?;

    // The declarations, up to the first `%%`.
    let mut declarations = Vec::new();
    let mut tags: HashMap<usize, String> = HashMap::new();
    let mut union = None;
    let mut expect = None;
    let mut level = 0u16;
    let mut pending = scanner.next()?;
    loop {
        let Some((item, line)) = pending else { return Err("gram.y has no rules".to_string()) };
        let at = |message: String| format!("gram.y line {line}: {message}");
        match item {
            Item::End => break,
            Item::Directive(directive) => {
                let directive: &'static str = match directive.as_str() {
                    "token" => "%token",
                    "left" => "%left",
                    "right" => "%right",
                    "nonassoc" => "%nonassoc",
                    "type" => "%type",
                    "expect" => "%expect",
                    "union" => "%union",
                    "pure-parser" | "name-prefix" | "locations" | "parse-param" | "lex-param" => "",
                    other => return Err(at(format!("the directive %{other} is not known"))),
                };
                let assoc = match directive {
                    "%left" => Some(Assoc::Left),
                    "%right" => Some(Assoc::Right),
                    "%nonassoc" => Some(Assoc::Nonassoc),
                    _ => None,
                };
                if assoc.is_some() {
                    level += 1;
                }
                let mut named = Vec::new();
                let mut tag = None;
                loop {
                    pending = scanner.next()?;
                    match &pending {
                        Some((Item::Tag(name), _)) => tag = Some(name.clone()),
                        Some((Item::Braces(body), _)) if directive == "%union" => {
                            union = Some(body.clone());
                        }
                        Some((Item::Word(name), _)) if directive == "%type" => {
                            let symbol = symbols.get(name);
                            if let Some(tag) = &tag {
                                tags.insert(symbol, tag.clone());
                            }
                        }
                        Some((Item::Word(word), _)) if directive == "%expect" => {
                            expect =
                                Some(word.parse().map_err(|_| at(format!("bad %expect {word}")))?);
                        }
                        Some((Item::Word(name) | Item::Char(name), _))
                            if directive == "%token" || assoc.is_some() =>
                        {
                            let symbol = symbols.token(name).map_err(at)?;
                            if let Some(tag) = &tag {
                                tags.insert(symbol, tag.clone());
                            }
                            if let Some(assoc) = assoc {
                                symbols.precedence[symbol] = (level, assoc);
                            }
                            named.push(symbol);
                        }
                        Some((Item::Directive(_) | Item::End, _)) | None => break,
                        _ => {}
                    }
                }
                if directive == "%token" || assoc.is_some() {
                    declarations.push(Declaration { directive, symbols: named });
                }
            }
            other => return Err(at(format!("unexpected {other:?} in the declarations"))),
        }
    }
    let expect = expect.ok_or("gram.y has no %expect")?;
    let union = union.ok_or("gram.y has no %union")?;

    // The rules, up to the second `%%`. A rule is `name: alternative | alternative ;`, an alternative is symbols, then an optional `%prec token`, then an optional action.
    let mut alternatives: Vec<Alternative> = Vec::new();
    let mut counts: HashMap<usize, usize> = HashMap::new();
    let mut pending = scanner.next()?;
    'rules: loop {
        let lhs = match pending {
            Some((Item::Word(name), line)) => {
                let lhs =
                    symbols.nonterminal(&name).map_err(|e| format!("gram.y line {line}: {e}"))?;
                match scanner.next()? {
                    Some((Item::Colon, _)) => lhs,
                    _ => return Err(format!("gram.y line {line}: {name} has no colon")),
                }
            }
            Some((Item::End, _)) | None => break 'rules,
            Some((other, line)) => {
                return Err(format!("gram.y line {line}: unexpected {other:?} before a rule"));
            }
        };
        loop {
            let mut rhs = Vec::new();
            let mut prec = None;
            let mut action = None;
            let mut first = None;
            let end = loop {
                let Some((item, line)) = scanner.next()? else {
                    return Err("gram.y ends in a rule".to_string());
                };
                first.get_or_insert(line);
                let at = |message: String| format!("gram.y line {line}: {message}");
                match item {
                    Item::Word(name) | Item::Char(name) if action.is_some() || prec.is_some() => {
                        return Err(at(format!("{name} after an action or a %prec")));
                    }
                    Item::Word(name) => rhs.push(symbols.get(&name)),
                    Item::Char(name) => rhs.push(symbols.token(&name).map_err(at)?),
                    Item::Directive(directive) if directive == "prec" => {
                        let token = match scanner.next()? {
                            Some((Item::Word(name) | Item::Char(name), _)) => {
                                symbols.token(&name).map_err(at)?
                            }
                            _ => return Err(at("%prec has no token".to_string())),
                        };
                        prec = Some(token);
                    }
                    Item::Braces(_) if action.is_some() => {
                        return Err(at("two actions".to_string()));
                    }
                    Item::Braces(body) => action = Some(body),
                    Item::Pipe | Item::Semicolon => break (item, line),
                    other => return Err(at(format!("unexpected {other:?} in a rule"))),
                }
            };
            let count = counts.entry(lhs).or_insert(0);
            *count += 1;
            let name = format!("{}.{count}", symbols.names[lhs]);
            let (end, line) = end;
            let line = first.unwrap_or(line);
            alternatives.push(Alternative { lhs, rhs, prec, name, action, line });
            if end == Item::Semicolon {
                break;
            }
        }
        pending = scanner.next()?;
    }

    // Every symbol that is not a token must have a rule.
    for (symbol, kind) in symbols.kind.iter().enumerate() {
        if *kind == Kind::Unknown {
            return Err(format!(
                "{} is used and has no rule and is not a token",
                symbols.names[symbol]
            ));
        }
    }
    let start = alternatives.first().ok_or("gram.y has no rules")?.lhs;

    // The final numbers: the tokens in the order they became tokens, then the nonterminals.
    let order: Vec<usize> =
        symbols.token_order.iter().chain(&symbols.nonterminal_order).copied().collect();
    let mut number = vec![0; order.len()];
    for (new, &old) in order.iter().enumerate() {
        number[old] = new;
    }
    let tokens = symbols.token_order.len();
    let names = order.iter().map(|&old| symbols.names[old].clone()).collect();
    let precedence = order.iter().map(|&old| symbols.precedence[old]).collect();

    let mut rules = vec![Rule {
        lhs: number[accept],
        rhs: vec![number[start], number[0]],
        prec: None,
        name: "$accept.1".to_string(),
        action: None,
        line: 0,
    }];
    for alternative in alternatives {
        let rhs: Vec<usize> = alternative.rhs.iter().map(|&old| number[old]).collect();
        let prec = alternative
            .prec
            .map(|old| number[old])
            .or_else(|| rhs.iter().rev().find(|&&symbol| symbol < tokens).copied());
        rules.push(Rule {
            lhs: number[alternative.lhs],
            rhs,
            prec,
            name: alternative.name,
            action: alternative.action,
            line: alternative.line,
        });
    }
    let declarations = declarations
        .into_iter()
        .map(|d| Declaration {
            directive: d.directive,
            symbols: d.symbols.iter().map(|&s| number[s]).collect(),
        })
        .collect();
    let tags = order.iter().map(|old| tags.remove(old)).collect();
    Ok(Grammar {
        symbols: names,
        tokens,
        precedence,
        rules,
        expect,
        tags,
        prologue,
        union,
        declarations,
    })
}

/// The header of the two files written from `gram.y`.
fn header(what: &str) -> String {
    format!(
        "/*\n * {what}\n *\n * Generated by `cargo xtask sql` from vendor/postgres-19/src/backend/parser/gram.y. Do not edit. `cargo xtask sql --check` runs in CI and fails if this file and gram.y disagree.\n */\n"
    )
}

/// Writes `gram.rules`: the grammar of `gram.y` with no C, which bison reads as it is.
pub(super) fn rules(text: &str) -> Result<String, String> {
    let grammar = read(text)?;
    let mut out = header(
        "The PostgreSQL grammar without its C: the tokens, the precedence declarations, every rule, every alternative and every %prec. Bison reads this file and makes the same states as from gram.y.",
    );
    let _ = writeln!(out, "\n%expect {}\n", grammar.expect);
    for declaration in &grammar.declarations {
        let mut line = declaration.directive.to_string();
        for &symbol in &declaration.symbols {
            let name = &grammar.symbols[symbol];
            if line.len() + 1 + name.len() > 100 {
                let _ = writeln!(out, "{line}");
                line = format!("{:width$}", "", width = declaration.directive.len());
            }
            line.push(' ');
            line.push_str(name);
        }
        let _ = writeln!(out, "{line}");
    }
    out.push_str("\n%%\n");
    let mut previous = None;
    for rule in &grammar.rules[1..] {
        if previous == Some(rule.lhs) {
            out.push_str("\t|");
        } else {
            if previous.is_some() {
                out.push_str("\t;\n");
            }
            let _ = write!(out, "\n{}:\n\t ", grammar.symbols[rule.lhs]);
            previous = Some(rule.lhs);
        }
        if rule.rhs.is_empty() {
            out.push_str(" /* empty */");
        }
        for &symbol in &rule.rhs {
            out.push(' ');
            out.push_str(&grammar.symbols[symbol]);
        }
        if let Some(prec) = explicit_prec(&grammar, rule) {
            let _ = write!(out, " %prec {}", grammar.symbols[prec]);
        }
        out.push('\n');
    }
    out.push_str("\t;\n");
    Ok(out)
}

/// The `%prec` of a rule when it is not the default, the last token on the right side.
fn explicit_prec(grammar: &Grammar, rule: &Rule) -> Option<usize> {
    let last = rule.rhs.iter().rev().find(|&&symbol| symbol < grammar.tokens).copied();
    rule.prec.filter(|&prec| Some(prec) != last)
}

/// Writes `productions.txt`: the name of each alternative and its right side, one to a line, in the order of `gram.y`.
pub(super) fn productions(text: &str) -> Result<String, String> {
    let grammar = read(text)?;
    let mut out = header(
        "Every alternative of the PostgreSQL grammar, as the rule name and the number of the alternative from 1, then its right side. The actions in `crates/rupg-sql/src/actions` and the glue use these names.",
    );
    out.push('\n');
    for rule in &grammar.rules[1..] {
        out.push_str(&rule.name);
        out.push(':');
        if rule.rhs.is_empty() {
            out.push_str(" /* empty */");
        }
        for &symbol in &rule.rhs {
            out.push(' ');
            out.push_str(&grammar.symbols[symbol]);
        }
        if let Some(prec) = explicit_prec(&grammar, rule) {
            let _ = write!(out, " %prec {}", grammar.symbols[prec]);
        }
        out.push('\n');
    }
    Ok(out)
}
