//! The configuration file `postgresql.conf`, as `guc-file.l` and `ProcessConfigFileInternal` in `guc.c` of PostgreSQL read it.
//!
//! A line is a name, an optional `=`, and a value. The value is a name, a number, a string in single quotes, or an unquoted string of letters, digits and `-._:/`. A `#` starts a comment. The directives `include`, `include_if_exists` and `include_dir` read other files in their place. When a parameter is on more than one line, the last line gives the value.
//!
//! The server reads the files at start, and an error stops the start. The view `pg_file_settings` reads the files again at each call, checks each line with the settings of the session, and changes nothing.

use std::collections::BTreeSet;
use std::path::Path;

use rupg_common::{Error, Result, SqlState};
use rupg_platform::Io;
use rupg_session::auth::FileSetting;
use rupg_session::guc::{self, Context, Settings};

use crate::hba::{absolute_location, canonical, conf_files};

/// `CONF_FILE_MAX_DEPTH`: the deepest nesting of included files.
const MAX_DEPTH: usize = 10;

/// `PG_AUTOCONF_FILENAME`: the file of `ALTER SYSTEM` in the data directory. It comes after the main file.
const AUTO_FILE: &str = "postgresql.auto.conf";

/// The number of syntax errors after which the server stops to read a file.
const MAX_ERRORS: usize = 100;

/// `ConfigVariable`: one line of a file, or one error.
#[derive(Clone, Debug, Default)]
struct Item {
    /// The name of the parameter, or `None` for an error.
    name: Option<String>,
    value: Option<String>,
    /// The file, or `None` for an error that has no file.
    file: Option<String>,
    line: u32,
    error: Option<String>,
    /// A line that a later line of the same name replaces, or an error.
    ignore: bool,
    applied: bool,
}

/// The tokens of `guc-file.l`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    Id,
    QualifiedId,
    String,
    UnquotedString,
    Integer,
    Real,
    Equals,
    Eol,
    Error,
    End,
}

/// `LETTER`: a letter, `_` or a byte with the high bit.
fn letter(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

/// `LETTER_OR_DIGIT`.
fn letter_or_digit(b: u8) -> bool {
    letter(b) || b.is_ascii_digit()
}

/// The end of the run of bytes from `from` that `f` takes.
fn run(s: &[u8], from: usize, f: impl Fn(u8) -> bool) -> usize {
    from + s.get(from..).unwrap_or_default().iter().take_while(|&&b| f(b)).count()
}

/// The length of `STRING` at the start of `s`, a string in single quotes with `''` and backslash escapes.
fn string_len(s: &[u8]) -> usize {
    let mut end = 0;
    let mut i = 1;
    while let Some(&b) = s.get(i) {
        match b {
            b'\n' => break,
            b'\\' => match s.get(i + 1) {
                Some(&next) if next != b'\n' => i += 2,
                _ => break,
            },
            b'\'' => {
                end = i + 1;
                if s.get(i + 1) != Some(&b'\'') {
                    break;
                }
                i += 2;
            }
            _ => i += 1,
        }
    }
    end
}

/// The length of `INTEGER` at the start of `s`: a sign, decimal digits or `0x` and hexadecimal digits, and the letters of a unit.
fn integer_len(s: &[u8]) -> usize {
    let start = usize::from(matches!(s.first(), Some(b'-' | b'+')));
    let units = |from: usize| run(s, from, |b| b.is_ascii_alphabetic());
    let digits = run(s, start, |b| b.is_ascii_digit());
    let decimal = if digits > start { units(digits) } else { 0 };
    let hex = if s.get(start..start + 2) == Some(b"0x") {
        let end = run(s, start + 2, |b| b.is_ascii_hexdigit());
        if end > start + 2 { units(end) } else { 0 }
    } else {
        0
    };
    decimal.max(hex)
}

/// The length of `REAL` at the start of `s`: a sign, digits, a point, digits, and an exponent.
fn real_len(s: &[u8]) -> usize {
    let start = usize::from(matches!(s.first(), Some(b'-' | b'+')));
    let point = run(s, start, |b| b.is_ascii_digit());
    if s.get(point) != Some(&b'.') {
        return 0;
    }
    let end = run(s, point + 1, |b| b.is_ascii_digit());
    if matches!(s.get(end), Some(b'e' | b'E')) {
        let sign = end + 1 + usize::from(matches!(s.get(end + 1), Some(b'-' | b'+')));
        let digits = run(s, sign, |b| b.is_ascii_digit());
        if digits > sign {
            return digits;
        }
    }
    end
}

/// The rule of `guc-file.l` that takes the longest text at the start of `s`, and the length. On a tie the first rule wins.
fn longest(s: &[u8]) -> (Token, usize) {
    let first = s[0];
    let mut rules = [(Token::Id, 0), (Token::QualifiedId, 0), (Token::String, 0)];
    let mut more = [(Token::UnquotedString, 0), (Token::Integer, 0), (Token::Real, 0)];
    if letter(first) {
        let id = run(s, 1, letter_or_digit);
        rules[0].1 = id;
        if s.get(id) == Some(&b'.') && s.get(id + 1).copied().is_some_and(letter) {
            rules[1].1 = run(s, id + 2, letter_or_digit);
        }
        more[0].1 = run(s, 1, |b| letter_or_digit(b) || b"-._:/".contains(&b));
    }
    if first == b'\'' {
        rules[2].1 = string_len(s);
    }
    more[1].1 = integer_len(s);
    more[2].1 = real_len(s);
    let last = [(Token::Equals, usize::from(first == b'=')), (Token::Error, 1)];
    let mut best = (Token::Error, 0);
    for (token, len) in rules.into_iter().chain(more).chain(last) {
        if len > best.1 {
            best = (token, len);
        }
    }
    best
}

/// The scanner of one file.
struct Lexer<'a> {
    text: &'a [u8],
    at: usize,
    /// `ConfigFileLineno`: the line of the next token, from 1.
    line: u32,
}

impl<'a> Lexer<'a> {
    /// The next token and its text. A space, a tab, a carriage return and a comment are not tokens.
    fn next(&mut self) -> (Token, &'a [u8]) {
        loop {
            let rest = &self.text[self.at..];
            match rest.first() {
                None => return (Token::End, b""),
                Some(b'\n') => {
                    self.at += 1;
                    self.line += 1;
                    return (Token::Eol, &rest[..1]);
                }
                Some(b' ' | b'\t' | b'\r') => self.at += 1,
                Some(b'#') => {
                    self.at += rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len())
                }
                Some(_) => {
                    let (token, len) = longest(rest);
                    self.at += len;
                    return (token, &rest[..len]);
                }
            }
        }
    }
}

/// `DeescapeQuotedString`: the text of a string in quotes, without the quotes, with `''` as one quote and the backslash escapes of C.
fn deescape(s: &[u8]) -> Vec<u8> {
    let body = &s[1..s.len() - 1];
    let mut out = Vec::with_capacity(body.len());
    let mut i = 0;
    while i < body.len() {
        match body[i] {
            b'\\' if i + 1 < body.len() => {
                i += 1;
                match body[i] {
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0c),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'0'..=b'7' => {
                        let mut value: u32 = 0;
                        let mut k = 0;
                        while k < 3 && matches!(body.get(i + k), Some(b'0'..=b'7')) {
                            value = (value << 3) + u32::from(body[i + k] - b'0');
                            k += 1;
                        }
                        i += k - 1;
                        out.push(value.to_le_bytes()[0]);
                    }
                    other => out.push(other),
                }
            }
            b'\'' if body.get(i + 1) == Some(&b'\'') => {
                out.push(b'\'');
                i += 1;
            }
            other => out.push(other),
        }
        i += 1;
    }
    out
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The reader of the files, with the list of lines and the log.
struct Reader<'a> {
    io: &'a dyn Io,
    /// The data directory, for a relative path of the main files.
    data_dir: Option<&'a str>,
    items: Vec<Item>,
    /// The messages that PostgreSQL logs at the level `LOG` at start.
    log: Vec<String>,
    /// For the view: stop at the first syntax error of a file, as PostgreSQL does at the level `DEBUG3`.
    quick: bool,
}

impl Reader<'_> {
    /// `record_config_file_error`.
    fn record(&mut self, error: String, file: Option<&str>, line: u32) {
        self.items.push(Item {
            file: file.map(str::to_owned),
            line,
            error: Some(error),
            ignore: true,
            ..Item::default()
        });
    }

    /// `ParseConfigFile`: reads one file. `strict` makes a file that does not open an error. `calling` is the file and the line of the directive that names the file.
    fn file(
        &mut self,
        name: &str,
        strict: bool,
        calling: Option<(&str, u32)>,
        depth: usize,
    ) -> bool {
        let (calling_file, calling_line) = calling.map_or((None, 0), |(f, l)| (Some(f), l));
        if name.trim_matches([' ', '\t', '\r', '\n']).is_empty() {
            self.log.push(format!("empty configuration file name: \"{name}\""));
            self.record("empty configuration file name".into(), calling_file, calling_line);
            return false;
        }
        if depth > MAX_DEPTH {
            self.log.push(format!(
                "could not open configuration file \"{name}\": maximum nesting depth exceeded"
            ));
            self.record("nesting depth exceeded".into(), calling_file, calling_line);
            return false;
        }
        let path = match calling_file {
            Some(outer) => absolute_location(name, outer),
            None if Path::new(name).is_absolute() => name.to_owned(),
            None => match self.data_dir {
                Some(dir) => canonical(&Path::new(dir).join(name)),
                None => name.to_owned(),
            },
        };
        if calling_file == Some(path.as_str()) {
            self.log.push(format!("configuration file recursion in \"{path}\""));
            self.record("configuration file recursion".into(), calling_file, calling_line);
            return false;
        }
        let text = match self.io.read_file(Path::new(&path)) {
            Ok(text) => text,
            Err(e) if strict => {
                let reason = e.message().rsplit_once("\": ").map_or(e.message(), |(_, r)| r);
                self.log.push(format!("could not open configuration file \"{path}\": {reason}"));
                self.record(format!("could not open file \"{path}\""), calling_file, calling_line);
                return false;
            }
            // PostgreSQL logs the missing file at the level `DEBUG1`.
            Err(_) => return true,
        };
        self.lines(&text, &path, depth)
    }

    /// `ParseConfigDirectory`: reads each file of the directory that ends in `.conf`, in the order of the names, and stops at the first file with an error.
    fn dir(&mut self, name: &str, calling: (&str, u32), depth: usize) -> bool {
        let files = match conf_files(self.io, calling.0, name) {
            Ok(files) => files,
            Err((text, error)) => {
                self.log.push(text);
                self.record(error, Some(calling.0), calling.1);
                return false;
            }
        };
        files.iter().all(|file| self.file(file, true, Some(calling), depth))
    }

    /// `ParseConfigFp`: the lines of the file `file`.
    fn lines(&mut self, text: &[u8], file: &str, depth: usize) -> bool {
        let mut lexer = Lexer { text, at: 0, line: 1 };
        let mut ok = true;
        let mut errors = 0;
        loop {
            let (mut token, mut tok) = lexer.next();
            if token == Token::End {
                break;
            }
            if token == Token::Eol {
                continue;
            }
            let parsed = 'line: {
                if !matches!(token, Token::Id | Token::QualifiedId) {
                    break 'line None;
                }
                let name = lossy(tok);
                (token, tok) = lexer.next();
                if token == Token::Equals {
                    (token, tok) = lexer.next();
                }
                let value = match token {
                    Token::String => lossy(&deescape(tok)),
                    Token::Id
                    | Token::Integer
                    | Token::Real
                    | Token::UnquotedString
                    | Token::QualifiedId => lossy(tok),
                    _ => break 'line None,
                };
                (token, tok) = lexer.next();
                match token {
                    Token::Eol => {}
                    // An item at the end of the file counts its line as if a newline ended it.
                    Token::End => lexer.line += 1,
                    _ => break 'line None,
                }
                Some((name, value))
            };
            let Some((name, value)) = parsed else {
                if matches!(token, Token::Eol | Token::End) {
                    let line = lexer.line - 1;
                    self.log.push(format!(
                        "syntax error in file \"{file}\" line {line}, near end of line"
                    ));
                    self.record("syntax error".into(), Some(file), line);
                } else {
                    let line = lexer.line;
                    self.log.push(format!(
                        "syntax error in file \"{file}\" line {line}, near token \"{}\"",
                        lossy(tok)
                    ));
                    self.record("syntax error".into(), Some(file), line);
                }
                ok = false;
                errors += 1;
                if errors >= MAX_ERRORS || self.quick {
                    self.log
                        .push(format!("too many syntax errors found, abandoning file \"{file}\""));
                    break;
                }
                while !matches!(token, Token::Eol | Token::End) {
                    token = lexer.next().0;
                }
                if token == Token::End {
                    break;
                }
                continue;
            };
            let line = lexer.line - 1;
            let at = (file, line);
            let directive = |word: &str| name.eq_ignore_ascii_case(word);
            if directive("include_dir") {
                ok &= self.dir(&value, at, depth + 1);
            } else if directive("include_if_exists") {
                ok &= self.file(&value, false, Some(at), depth + 1);
            } else if directive("include") {
                ok &= self.file(&value, true, Some(at), depth + 1);
            } else {
                self.items.push(Item {
                    name: Some(name),
                    value: Some(value),
                    file: Some(file.to_owned()),
                    line,
                    ..Item::default()
                });
            }
            if token == Token::End {
                break;
            }
        }
        ok
    }

    /// The main file and then the file of `ALTER SYSTEM` in the data directory. An error gives the name of the file that PostgreSQL names in the message.
    fn read_all(&mut self, main: &str) -> std::result::Result<(), String> {
        if !self.file(main, true, None, 0) {
            return Err(main.to_owned());
        }
        // The error names the file of `ALTER SYSTEM` without its directory.
        if self.data_dir.is_some() && !self.file(AUTO_FILE, false, None, 0) {
            return Err(AUTO_FILE.to_owned());
        }
        Ok(())
    }
}

/// `ProcessConfigFileInternal` after the files are read: marks the lines that a later line of the same name replaces, checks the names, and then gives each line to `set`. `set` sets a value or checks it. An error in a name stops before any value. The result is the file of the last error, if there is one.
fn process(
    items: &mut Vec<Item>,
    settings: &Settings,
    log: &mut Vec<String>,
    mut set: impl FnMut(&str, &str, &str, u32) -> Result<bool>,
) -> Option<String> {
    let mut error = None;
    let mut seen = BTreeSet::new();
    for i in 0..items.len() {
        let Some(name) = items[i].name.clone().filter(|_| !items[i].ignore) else { continue };
        if settings.exists(&name) {
            let key = guc::find(&name)
                .map_or_else(|| name.to_ascii_lowercase(), |p| p.name.to_ascii_lowercase());
            if !seen.insert(key) {
                for earlier in &mut items[..i] {
                    if !earlier.ignore && earlier.name.as_deref() == Some(name.as_str()) {
                        earlier.ignore = true;
                    }
                }
            }
        } else if !guc::valid_custom_name(&name) {
            let item = &mut items[i];
            let file = item.file.clone().unwrap_or_default();
            log.push(format!(
                "unrecognized configuration parameter \"{name}\" in file \"{file}\" line {}",
                item.line
            ));
            item.error = Some("unrecognized configuration parameter".into());
            error = Some(file);
        }
    }
    if error.is_some() {
        return error;
    }
    // A parameter of the server start that left the file keeps its value until a restart.
    for name in settings.file_names() {
        let gone = guc::find(&name).is_some_and(|p| {
            matches!(p.context, Context::Internal | Context::Postmaster)
                && !seen.contains(&p.name.to_ascii_lowercase())
        });
        if gone {
            let text =
                format!("parameter \"{name}\" cannot be changed without restarting the server");
            log.push(text.clone());
            items.push(Item { error: Some(text), ignore: true, ..Item::default() });
            error = Some(String::new());
        }
    }
    for item in items.iter_mut().filter(|item| !item.ignore) {
        let (Some(name), Some(value), Some(file)) = (&item.name, &item.value, &item.file) else {
            continue;
        };
        match set(name, value, file, item.line) {
            Ok(applied) => item.applied = applied,
            Err(e) => {
                log.push(e.message().to_owned());
                item.error = Some("setting could not be applied".into());
                error = Some(file.clone());
            }
        }
    }
    error
}

/// The configuration files of a server: the main file, and the data directory with the file of `ALTER SYSTEM`.
#[derive(Clone, Debug)]
pub(crate) struct ConfFiles {
    main: String,
    data_dir: Option<String>,
}

/// The error that stops the start, as `ProcessConfigFileInternal` gives it in the postmaster.
fn contains_errors(file: &str) -> Error {
    Error::new(
        SqlState::CONFIG_FILE_ERROR,
        format!("configuration file \"{file}\" contains errors"),
    )
}

/// `SelectConfigFiles`: finds the configuration file, reads it into `settings`, and sets `config_file`, `data_directory`, `hba_file`, `ident_file` and `hosts_file`. `config_dir` is the directory of `-D`. The main file is `config_file` when it is set, else `postgresql.conf` in `config_dir`. With neither, the server reads no file and the result is `None`. The messages of the files go to `log`.
///
/// # Errors
///
/// `F0000` for a file with an error.
pub(crate) fn select(
    settings: &mut Settings,
    config_dir: Option<&str>,
    io: &dyn Io,
    log: &mut Vec<String>,
) -> Result<Option<ConfFiles>> {
    let named = settings.get("config_file").filter(|file| !file.is_empty());
    let main = match (named, config_dir) {
        (Some(file), _) => file,
        (None, Some(dir)) => format!("{dir}/postgresql.conf"),
        (None, None) => return Ok(None),
    };
    settings.set_override("config_file", &main)?;
    // The first pass reads only `data_directory`, because the file of `ALTER SYSTEM` is in the data directory.
    let mut first = Reader { io, data_dir: None, items: Vec::new(), log: Vec::new(), quick: false };
    let read = first.read_all(&main);
    log.append(&mut first.log);
    read.map_err(|file| contains_errors(&file))?;
    if let Some(item) = first
        .items
        .iter()
        .rev()
        .find(|item| !item.ignore && item.name.as_deref() == Some("data_directory"))
    {
        settings.set_file("data_directory", item.value.as_deref())?;
    }
    let data_dir = settings
        .get("data_directory")
        .filter(|dir| !dir.is_empty())
        .or_else(|| config_dir.map(str::to_owned))
        .map(|dir| canonical(Path::new(&dir)));
    if let Some(dir) = &data_dir {
        settings.set_override("data_directory", dir)?;
    }
    let mut reader = Reader {
        io,
        data_dir: data_dir.as_deref(),
        items: Vec::new(),
        log: Vec::new(),
        quick: false,
    };
    let read = reader.read_all(&main);
    let mut items = std::mem::take(&mut reader.items);
    log.append(&mut reader.log);
    read.map_err(|file| contains_errors(&file))?;
    let check = settings.clone();
    let error = process(&mut items, &check, log, |name, value, file, line| {
        if guc::find(name).is_some_and(|p| p.context == Context::Internal) {
            return Err(Error::new(
                SqlState::CANT_CHANGE_RUNTIME_PARAM,
                format!("parameter \"{name}\" cannot be changed"),
            ));
        }
        let applied = settings.set_file(name, Some(value))?;
        if applied {
            settings.set_location(name, file, i32::try_from(line).unwrap_or(i32::MAX));
        }
        Ok(applied)
    });
    if let Some(file) = error {
        return Err(contains_errors(if file.is_empty() { &main } else { &file }));
    }
    for (name, file) in [
        ("hba_file", "pg_hba.conf"),
        ("ident_file", "pg_ident.conf"),
        ("hosts_file", "pg_hosts.conf"),
    ] {
        if guc::find(name).is_none() {
            continue;
        }
        let path = settings
            .get(name)
            .filter(|path| !path.is_empty())
            .or_else(|| config_dir.map(|dir| format!("{dir}/{file}")));
        if let Some(path) = path {
            settings.set_override(name, &path)?;
        }
    }
    Ok(Some(ConfFiles { main, data_dir }))
}

/// `show_all_file_settings`: the lines of the files as they are now, each checked with the settings of the session. A file with a syntax error gives its lines so far and the error, and no line is applied.
pub(crate) fn file_settings(
    io: &dyn Io,
    files: &ConfFiles,
    settings: &Settings,
) -> Vec<FileSetting> {
    let mut reader = Reader {
        io,
        data_dir: files.data_dir.as_deref(),
        items: Vec::new(),
        log: Vec::new(),
        quick: true,
    };
    let mut items = if reader.read_all(&files.main).is_ok() {
        let mut items = std::mem::take(&mut reader.items);
        process(&mut items, settings, &mut reader.log, |name, value, _, _| {
            settings.check_file(name, value).map(|()| true)
        });
        items
    } else {
        reader.items
    };
    items
        .drain(..)
        .map(|item| FileSetting {
            file: item.file,
            line: i32::try_from(item.line).unwrap_or(i32::MAX),
            name: item.name,
            setting: item.value,
            applied: item.applied,
            error: item.error,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(text: &str) -> Vec<(Token, String)> {
        let mut lexer = Lexer { text: text.as_bytes(), at: 0, line: 1 };
        let mut out = Vec::new();
        loop {
            let (token, tok) = lexer.next();
            if token == Token::End {
                return out;
            }
            out.push((token, lossy(tok)));
        }
    }

    #[test]
    fn the_tokens_of_guc_file() {
        use Token::*;
        let t = |text: &str| tokens(text).into_iter().map(|(t, _)| t).collect::<Vec<_>>();
        assert_eq!(t("work_mem = 4MB # comment\n"), [Id, Equals, Integer, Eol]);
        assert_eq!(t("a.b 0x1fkB -1.5e3 .5 -"), [QualifiedId, Integer, Real, Real, Error]);
        assert_eq!(t("x 'it''s' en_US.UTF-8 a.b.c"), [Id, String, UnquotedString, UnquotedString]);
        assert_eq!(tokens("p 'a''")[1], (String, "'a'".into()));
        // An unquoted string starts with a letter, so a path needs quotes.
        assert_eq!(t("v /tmp"), [Id, Error, Id]);
        assert_eq!(t("1.5GB"), [Real, Id]);
    }

    #[test]
    fn the_escapes_of_a_string() {
        assert_eq!(deescape(b"'a''b\\n\\101\\q'"), b"a'b\nAq");
        assert_eq!(deescape(b"''"), b"");
    }

    #[test]
    fn lines_and_errors() {
        let io = rupg_platform::sim::SimIo::new(0);
        let mut reader =
            Reader { io: &io, data_dir: None, items: Vec::new(), log: Vec::new(), quick: false };
        let text = b"# head\nwork_mem = '8MB'\nbad line here\nport 5433";
        assert!(!reader.lines(text, "/d/postgresql.conf", 0));
        let lines: Vec<_> = reader
            .items
            .iter()
            .map(|i| (i.name.clone(), i.value.clone(), i.line, i.error.clone()))
            .collect();
        assert_eq!(
            lines,
            [
                (Some("work_mem".into()), Some("8MB".into()), 2, None),
                (None, None, 3, Some("syntax error".into())),
                (Some("port".into()), Some("5433".into()), 4, None),
            ]
        );
        assert_eq!(
            reader.log,
            ["syntax error in file \"/d/postgresql.conf\" line 3, near token \"here\""]
        );
    }
}
