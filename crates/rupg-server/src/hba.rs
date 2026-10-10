//! The host rules and the user maps: the text of `pg_hba.conf` and `pg_ident.conf`, as `hba.c` of PostgreSQL reads them.
//!
//! Both texts go through one tokenizer. A token ends at a space, a tab or a comma, double quotes keep a token together, `""` in quotes is one quote, an unquoted `#` starts a comment, and a backslash at the end of a line joins the next line. A field is a list of tokens with commas between them. `@file` reads the tokens of a file into the field, and a line of two fields that starts with `include`, `include_if_exists` or `include_dir` reads the lines of other files.
//!
//! The server reads the rules when it starts. Each error goes to the log with the line of the file as its context, and the rest of the text is still checked, so one start shows every error. Host rules with an error or with no entry stop the start. User maps with an error only go to the log, and then no map matches. The views `pg_hba_file_rules` and `pg_ident_file_mappings` read the text again at each call, and show each line with the error that PostgreSQL keeps for it.
//!
//! The methods are those of PostgreSQL. `gss`, `sspi`, `pam`, `bsd` and `ldap` are methods of builds with other libraries, and a line with one of them gets the error of such a build. `oauth` needs a validator library, which rupg cannot load yet, so a line with it gets the error of a server with no `oauth_validator_libraries`.
//!
//! A token that starts with a slash is a regular expression, also in quotes. The pattern after the slash uses the advanced syntax of PostgreSQL and the C collation, and it matches a part of the name, so a full match needs `^` and `$`. A pattern that does not compile is an error on its line. In a user map, `\1` in the user name gets the text of the first group of the match on the system user.
//!
//! The files come through the `Io` trait and the host name lookups through the `Net` trait of rupg-platform, so the simulation can run them.
//!
//! Lifted from `crates/rudb-server/src/hba.rs` of tamnd/rudb at f5f7065a.

use std::cell::RefCell;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Component, Path};

use rupg_common::{Result, SqlState, SqlState as S};
use rupg_platform::{Io, Net};
use rupg_session::auth::{
    HbaFields, HbaRule, IdentFields, IdentMapping, auth_compile, auth_search,
};

/// `CONF_FILE_MAX_DEPTH`: how deep files can include files.
const MAX_DEPTH: usize = 10;

/// The text of `gai_strerror(EAI_NONAME)` in glibc, which PostgreSQL shows for a mask that is not an address.
const EAI_NONAME_TEXT: &str = "Name or service not known";

/// The roles that the rules and the maps check. The server has one role at this step, the superuser.
#[derive(Debug, Clone)]
pub(crate) struct Roles {
    pub(crate) superuser: String,
}

impl Roles {
    /// True when the role exists.
    fn exists(&self, role: &str) -> bool {
        role == self.superuser
    }

    /// `is_member`: the role `user` is a member of the role `group`, directly or through other roles. A role is a member of itself. A superuser is not a member of every role here.
    fn is_member(&self, user: &str, group: &str) -> bool {
        self.exists(user) && self.exists(group) && user == group
    }
}

/// One token of a field.
#[derive(Debug, Clone)]
struct Token {
    text: String,
    /// The token had a double quote before its first character. A quoted token is never a keyword, a group or a file.
    quoted: bool,
    /// The token is a regular expression that [`Token::compile`] compiled.
    regex: bool,
}

impl Token {
    fn new(text: String, quoted: bool) -> Token {
        Token { text, quoted, regex: false }
    }

    /// `token_is_keyword`.
    fn keyword(&self, word: &str) -> bool {
        !self.quoted && self.text == word
    }

    /// The role of a `+role` token, `token_is_member_check`.
    fn group(&self) -> Option<&str> {
        self.text.strip_prefix('+').filter(|_| !self.quoted)
    }

    /// `regcomp_auth_token`: a token that starts with a slash is a regular expression.
    fn compile(&mut self) -> Result<(), String> {
        let Some(pattern) = self.text.strip_prefix('/') else { return Ok(()) };
        auth_compile(pattern)
            .map_err(|text| format!("invalid regular expression \"{pattern}\": {text}"))?;
        self.regex = true;
        Ok(())
    }

    /// The pattern of a regular expression, after its slash.
    fn pattern(&self) -> Option<&str> {
        self.text.strip_prefix('/').filter(|_| self.regex)
    }

    /// `regexec_auth_token` with no groups: the regular expression matches a part of `name`. An error of the match is no match.
    fn finds(&self, name: &str) -> bool {
        self.pattern().is_some_and(|pattern| matches!(auth_search(pattern, name), Ok(Some(_))))
    }
}

/// One line of a file after the tokenizer, `TokenizedAuthLine`.
#[derive(Debug)]
struct TokenLine {
    file: String,
    number: usize,
    raw: String,
    fields: Vec<Vec<Token>>,
    /// The error of the line, which is in the log already. The view `pg_hba_file_rules` shows it.
    error: Option<String>,
}

/// A message for the log at the level `LOG`, with its `HINT` and `CONTEXT` lines.
fn message(text: &str, hint: Option<&str>, context: &[String]) -> String {
    let mut out = text.to_owned();
    if let Some(hint) = hint {
        out.push_str("\nHINT:  ");
        out.push_str(hint);
    }
    for (at, line) in context.iter().enumerate() {
        out.push_str(if at == 0 { "\nCONTEXT:  " } else { "\n" });
        out.push_str(line);
    }
    out
}

/// Where the errors of the parse of one line go: the log, and the text that PostgreSQL keeps for the views, `err_msg`.
struct Report<'a> {
    log: &'a mut Vec<String>,
    context: [String; 1],
    /// The text of the last error or warning of the line.
    error: Option<String>,
}

impl<'a> Report<'a> {
    fn new(line: &TokenLine, log: &'a mut Vec<String>) -> Report<'a> {
        Report { log, context: [line_context(line.number, &line.file)], error: None }
    }

    /// A warning: the line still loads, but it can never match.
    fn warn(&mut self, text: &str, hint: Option<&str>) {
        self.log.push(message(text, hint, &self.context));
        self.error = Some(text.to_owned());
    }

    /// An error: the line does not load.
    fn fail<T>(&mut self, text: &str, hint: Option<&str>) -> Option<T> {
        self.warn(text, hint);
        None
    }

    /// The error of an option.
    fn option<T>(&mut self, error: OptionError) -> Option<T> {
        self.log.push(message(&error.text, None, &self.context));
        self.error = error.view;
        None
    }
}

/// The context of an error on a line, `line N of configuration file "F"`.
fn line_context(number: usize, file: &str) -> String {
    format!("line {number} of configuration file \"{file}\"")
}

/// The tokenizer of the authentication files, with the log of the errors that it finds.
struct Tokenizer<'a> {
    io: &'a dyn Io,
    log: &'a mut Vec<String>,
    /// The lines that the tokenizer is in, the innermost last, for the context of an error.
    stack: Vec<String>,
}

/// A file that did not open: the error for the line, and whether the file is not there.
struct OpenError {
    text: String,
    missing: bool,
}

impl Tokenizer<'_> {
    fn context(&self) -> Vec<String> {
        self.stack.iter().rev().cloned().collect()
    }

    fn error(&mut self, text: &str) {
        let line = message(text, None, &self.context());
        self.log.push(line);
    }

    /// `open_auth_file`.
    fn open(&mut self, path: &str, depth: usize) -> Result<String, OpenError> {
        if depth > MAX_DEPTH {
            let text = format!("could not open file \"{path}\": maximum nesting depth exceeded");
            self.error(&text);
            return Err(OpenError { text, missing: false });
        }
        match self.io.read_file(Path::new(path)) {
            Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
            Err(e) => {
                let text = e.message().to_owned();
                self.error(&text);
                Err(OpenError { text, missing: e.state() == S::UNDEFINED_FILE })
            }
        }
    }

    /// `tokenize_auth_file`: adds the lines of `text`, the file `file`, to `lines`.
    fn file(&mut self, file: &str, text: &str, lines: &mut Vec<TokenLine>, depth: usize) {
        let mut number = 1;
        let mut rest = text;
        while !rest.is_empty() {
            // One line with its continuations. The backslash must come after the place of the last continuation, so `\\` and two line ends do not join three lines.
            let mut buf = String::new();
            let mut continuations = 0;
            let mut last = 0;
            while !rest.is_empty() {
                let end = rest.find('\n').map_or(rest.len(), |at| at + 1);
                buf.push_str(rest[..end].trim_end_matches(['\n', '\r']));
                rest = &rest[end..];
                if buf.len() > last && buf.ends_with('\\') {
                    buf.pop();
                    last = buf.len();
                    continuations += 1;
                    continue;
                }
                break;
            }
            self.stack.push(line_context(number, file));
            self.line(file, number, buf, lines, depth);
            self.stack.pop();
            number += continuations + 1;
        }
    }

    /// The fields of one line, and the include directives.
    fn line(
        &mut self,
        file: &str,
        number: usize,
        raw: String,
        lines: &mut Vec<TokenLine>,
        depth: usize,
    ) {
        let mut fields = Vec::new();
        let mut error = None;
        let mut at = 0;
        let bytes = raw.as_bytes();
        while at < bytes.len() && error.is_none() {
            let field = self.field(file, bytes, &mut at, depth, &mut error);
            if !field.is_empty() {
                fields.push(field);
            }
        }
        if fields.is_empty() && error.is_none() {
            return;
        }
        if error.is_none() && fields.len() == 2 {
            let first = fields[0][0].text.clone();
            let second = fields[1][0].text.clone();
            match first.as_str() {
                "include" | "include_if_exists" => {
                    let missing_ok = first == "include_if_exists";
                    match self.include(file, &second, lines, depth + 1, missing_ok) {
                        Ok(()) => return,
                        Err(text) => error = Some(text),
                    }
                }
                "include_dir" => match self.include_dir(file, &second, lines, depth) {
                    Ok(()) => return,
                    Err(text) => error = Some(text),
                },
                _ => {}
            }
        }
        lines.push(TokenLine { file: file.to_owned(), number, raw, fields, error });
    }

    /// `next_field_expand`: the tokens of one field, with `@file` read in.
    fn field(
        &mut self,
        file: &str,
        line: &[u8],
        at: &mut usize,
        depth: usize,
        error: &mut Option<String>,
    ) -> Vec<Token> {
        let mut tokens = Vec::new();
        while let Some((text, quoted, comma)) = next_token(line, at) {
            if !quoted && text.len() > 1 && text.starts_with('@') {
                self.expand(file, &text[1..], &mut tokens, depth + 1, error);
            } else {
                tokens.push(Token::new(text, quoted));
            }
            if !comma || error.is_some() {
                break;
            }
        }
        tokens
    }

    /// `tokenize_expand_file`: the tokens of every line of the file `name` into `tokens`.
    fn expand(
        &mut self,
        outer: &str,
        name: &str,
        tokens: &mut Vec<Token>,
        depth: usize,
        error: &mut Option<String>,
    ) {
        let path = absolute_location(name, outer);
        let text = match self.open(&path, depth) {
            Ok(text) => text,
            Err(failed) => {
                *error = Some(failed.text);
                return;
            }
        };
        let mut inner = Vec::new();
        self.file(&path, &text, &mut inner, depth);
        for line in inner {
            if line.error.is_some() {
                *error = Some(format!("error in file \"{}\"", line.file));
                break;
            }
            tokens.extend(line.fields.into_iter().flatten());
        }
    }

    /// `tokenize_include_file`.
    fn include(
        &mut self,
        outer: &str,
        name: &str,
        lines: &mut Vec<TokenLine>,
        depth: usize,
        missing_ok: bool,
    ) -> Result<(), String> {
        let path = absolute_location(name, outer);
        match self.open(&path, depth) {
            Ok(text) => {
                self.file(&path, &text, lines, depth);
                Ok(())
            }
            Err(failed) if failed.missing && missing_ok => {
                self.error(&format!("skipping missing authentication file \"{path}\""));
                Ok(())
            }
            Err(failed) => Err(failed.text),
        }
    }

    /// The `include_dir` directive: each file of the directory that ends in `.conf`, in the order of the names.
    fn include_dir(
        &mut self,
        outer: &str,
        name: &str,
        lines: &mut Vec<TokenLine>,
        depth: usize,
    ) -> Result<(), String> {
        let files = match conf_files(self.io, outer, name) {
            Ok(files) => files,
            Err((text, error)) => {
                self.error(&text);
                return Err(error);
            }
        };
        let mut errors = Vec::new();
        for path in files {
            if let Err(text) = self.include(outer, &path, lines, depth + 1, false) {
                errors.push(text);
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(errors.join("\n")) }
    }
}

/// `GetConfFilesInDir`: the files of the directory `name` that end in `.conf`, in the order of their names. `name` is relative to the directory of the file `outer`. An error is the text of the log line and the short text of the error.
fn conf_files(io: &dyn Io, outer: &str, name: &str) -> Result<Vec<String>, (String, String)> {
    if name.trim_matches([' ', '\t', '\r', '\n']).is_empty() {
        return Err((
            format!("empty configuration directory name: \"{name}\""),
            "empty configuration directory name".to_owned(),
        ));
    }
    let dir = absolute_location(name, outer);
    let entries = io.read_dir(Path::new(&dir)).map_err(|e| {
        let text = e.message();
        // The platform gives the stat error of one entry as it is, and the error of the directory with its own words.
        if let Some(rest) = text.strip_prefix("could not stat file \"") {
            let path = rest.split_once('"').map_or(rest, |(path, _)| path);
            return (text.to_owned(), format!("could not stat file \"{path}\""));
        }
        let reason = text.rsplit_once("\": ").map_or("", |(_, reason)| reason);
        (
            format!("could not open configuration directory \"{dir}\": {reason}"),
            format!("could not open directory \"{dir}\""),
        )
    })?;
    let mut files = Vec::new();
    for (file, is_dir) in entries {
        if file.len() < 6 || file.starts_with('.') || !file.ends_with(".conf") || is_dir {
            continue;
        }
        files.push(canonical(&Path::new(&dir).join(&file)));
    }
    files.sort();
    Ok(files)
}

/// `next_token`: the next token of `line` from `at`, whether it started with a quote, and whether a comma ends it. `None` at the end of the line or at a comment.
fn next_token(line: &[u8], at: &mut usize) -> Option<(String, bool, bool)> {
    let blank = |c: u8| c == b' ' || c == b'\t';
    while *at < line.len() && (blank(line[*at]) || line[*at] == b',') {
        *at += 1;
    }
    let mut buf = Vec::new();
    let (mut in_quote, mut was_quote, mut saw_quote) = (false, false, false);
    let mut initial_quote = false;
    let mut comma = false;
    while *at < line.len() && (!blank(line[*at]) || in_quote) {
        let c = line[*at];
        if c == b'#' && !in_quote {
            *at = line.len();
            break;
        }
        if c == b',' && !in_quote {
            comma = true;
            break;
        }
        if c != b'"' || was_quote {
            buf.push(c);
        }
        was_quote = in_quote && c == b'"' && !was_quote;
        if c == b'"' {
            in_quote = !in_quote;
            saw_quote = true;
            if buf.is_empty() {
                initial_quote = true;
            }
        }
        *at += 1;
    }
    (saw_quote || !buf.is_empty())
        .then(|| (String::from_utf8_lossy(&buf).into_owned(), initial_quote, comma))
}

/// `AbsoluteConfigLocation`: a path relative to the directory of the file that names it.
fn absolute_location(location: &str, calling: &str) -> String {
    let path = Path::new(location);
    if path.is_absolute() {
        return location.to_owned();
    }
    let base = Path::new(calling).parent().unwrap_or(Path::new("/"));
    canonical(&base.join(path))
}

/// `canonicalize_path`, which works on the text and does not follow links.
pub(crate) fn canonical(path: &Path) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let absolute = path.is_absolute();
    let mut ups = 0;
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str().unwrap_or_default()),
            Component::ParentDir => {
                if parts.pop().is_none() && !absolute {
                    ups += 1;
                }
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    let mut out = if absolute { "/".to_owned() } else { "../".repeat(ups) };
    out.push_str(&parts.join("/"));
    if out.is_empty() {
        out.push('.');
    }
    if out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    out
}

/// Where the text of the rules or the maps comes from.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Source<'a> {
    /// A file on disk, `hba_file` or `ident_file`.
    File(&'a str),
    /// The stored text, with the name of the file that it stands for.
    Text { name: &'a str, text: &'a str },
}

/// Reads one authentication file from its top, `open_auth_file` and `tokenize_auth_file`.
fn tokenize(io: &dyn Io, source: Source<'_>, log: &mut Vec<String>) -> Option<Vec<TokenLine>> {
    let mut tokenizer = Tokenizer { io, log, stack: Vec::new() };
    let (name, text) = match source {
        Source::File(path) => (path, tokenizer.open(path, 0).ok()?),
        Source::Text { name, text } => (name, text.to_owned()),
    };
    let mut lines = Vec::new();
    tokenizer.file(name, &text, &mut lines, 0);
    Some(lines)
}

/// The kind of connection of a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Local,
    Host,
    HostSsl,
    HostNoSsl,
    HostGssEnc,
    HostNoGssEnc,
}

impl Kind {
    /// The name of the connection type in the rules.
    fn name(self) -> &'static str {
        match self {
            Kind::Local => "local",
            Kind::Host => "host",
            Kind::HostSsl => "hostssl",
            Kind::HostNoSsl => "hostnossl",
            Kind::HostGssEnc => "hostgssenc",
            Kind::HostNoGssEnc => "hostnogssenc",
        }
    }
}

/// The address of a `host` line.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Address {
    /// A `local` line has none.
    None,
    All,
    SameHost,
    SameNet,
    Mask(IpAddr, IpAddr),
    /// A host name, or a suffix of host names when it starts with a dot.
    Name(String),
}

/// The authentication method of a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Trust,
    Reject,
    Ident,
    Peer,
    Password,
    Md5,
    Scram,
    Cert,
    OAuth,
}

impl Method {
    /// `auth_failed`: the text of the error after a failed authentication, and its SQLSTATE.
    pub(crate) fn failed(self, user: &str) -> (SqlState, String) {
        let state = S::INVALID_AUTHORIZATION_SPECIFICATION;
        match self {
            Method::Reject => {
                (state, format!("authentication failed for user \"{user}\": host rejected"))
            }
            Method::Trust => {
                (state, format!("\"trust\" authentication failed for user \"{user}\""))
            }
            Method::Ident => (state, format!("Ident authentication failed for user \"{user}\"")),
            Method::Peer => (state, format!("Peer authentication failed for user \"{user}\"")),
            Method::Password | Method::Md5 | Method::Scram => {
                (S::INVALID_PASSWORD, format!("password authentication failed for user \"{user}\""))
            }
            Method::Cert => {
                (state, format!("certificate authentication failed for user \"{user}\""))
            }
            Method::OAuth => {
                (state, format!("OAuth bearer authentication failed for user \"{user}\""))
            }
        }
    }

    /// The name of the method in the rules.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Method::Trust => "trust",
            Method::Reject => "reject",
            Method::Ident => "ident",
            Method::Peer => "peer",
            Method::Password => "password",
            Method::Md5 => "md5",
            Method::Scram => "scram-sha-256",
            Method::Cert => "cert",
            Method::OAuth => "oauth",
        }
    }
}

/// The `clientcert` option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClientCert {
    Off,
    VerifyCa,
    VerifyFull,
}

/// The `clientname` option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClientName {
    Cn,
    Dn,
}

/// One line of the host rules, `HbaLine`.
#[derive(Debug, Clone)]
pub(crate) struct HbaLine {
    pub(crate) file: String,
    pub(crate) number: usize,
    pub(crate) raw: String,
    pub(crate) kind: Kind,
    databases: Vec<Token>,
    roles: Vec<Token>,
    address: Address,
    pub(crate) method: Method,
    pub(crate) map: Option<String>,
    pub(crate) clientcert: ClientCert,
    pub(crate) clientname: ClientName,
}

impl HbaLine {
    /// The parts of the line that the view `pg_hba_file_rules` shows, `fill_hba_line` and `get_hba_options`.
    fn fields(&self) -> HbaFields {
        let keyword = |word: &str| (Some(word.to_owned()), None);
        let (address, netmask) = match &self.address {
            Address::None => (None, None),
            Address::All => keyword("all"),
            Address::SameHost => keyword("samehost"),
            Address::SameNet => keyword("samenet"),
            Address::Mask(address, mask) => (Some(address.to_string()), Some(mask.to_string())),
            Address::Name(name) => (Some(name.clone()), None),
        };
        let mut options = Vec::new();
        if let Some(map) = &self.map {
            options.push(format!("map={map}"));
        }
        match self.clientcert {
            ClientCert::Off => {}
            ClientCert::VerifyCa => options.push("clientcert=verify-ca".to_owned()),
            ClientCert::VerifyFull => options.push("clientcert=verify-full".to_owned()),
        }
        let texts = |tokens: &[Token]| tokens.iter().map(|token| token.text.clone()).collect();
        HbaFields {
            kind: self.kind.name().to_owned(),
            databases: texts(&self.databases),
            users: texts(&self.roles),
            address,
            netmask,
            method: self.method.name().to_owned(),
            options,
        }
    }
}

/// The methods that only other builds of PostgreSQL have.
const OTHER_BUILDS: [&str; 5] = ["gss", "sspi", "pam", "bsd", "ldap"];

/// The options of methods that only other builds have, with the text of the valid methods.
const OTHER_OPTIONS: [(&str, &str); 19] = [
    ("pamservice", "pam"),
    ("pam_use_hostname", "pam"),
    ("ldapurl", "ldap"),
    ("ldaptls", "ldap"),
    ("ldapscheme", "ldap"),
    ("ldapserver", "ldap"),
    ("ldapport", "ldap"),
    ("ldapbinddn", "ldap"),
    ("ldapbindpasswd", "ldap"),
    ("ldapsearchattribute", "ldap"),
    ("ldapsearchfilter", "ldap"),
    ("ldapbasedn", "ldap"),
    ("ldapprefix", "ldap"),
    ("ldapsuffix", "ldap"),
    ("krb_realm", "gssapi and sspi"),
    ("include_realm", "gssapi and sspi"),
    ("compat_realm", "sspi"),
    ("upn_username", "sspi"),
    ("radiusservers", "radius"),
];

/// `parse_hba_line`. An error goes to the log of `report` and gives `None`. `ssl` is the setting `ssl`.
fn parse_hba_line(line: &TokenLine, ssl: bool, report: &mut Report<'_>) -> Option<HbaLine> {
    let mut fields = line.fields.iter();
    let kind_field = fields.next()?;
    if kind_field.len() > 1 {
        return report.fail(
            "multiple values specified for connection type",
            Some("Specify exactly one connection type per line."),
        );
    }
    let kind = match kind_field[0].text.as_str() {
        "local" => Kind::Local,
        "host" => Kind::Host,
        "hostssl" => {
            if !ssl {
                // The line still loads. It can never match.
                report.warn(
                    "hostssl record cannot match because SSL is disabled",
                    Some("Set \"ssl = on\" in postgresql.conf."),
                );
            }
            Kind::HostSsl
        }
        "hostnossl" => Kind::HostNoSsl,
        "hostgssenc" => {
            report.warn(
                "hostgssenc record cannot match because GSSAPI is not supported by this build",
                None,
            );
            Kind::HostGssEnc
        }
        "hostnogssenc" => Kind::HostNoGssEnc,
        other => return report.fail(&format!("invalid connection type \"{other}\""), None),
    };
    let Some(databases) = fields.next() else {
        return report.fail("end-of-line before database specification", None);
    };
    let mut databases = databases.clone();
    for token in &mut databases {
        if let Err(text) = token.compile() {
            return report.fail(&text, None);
        }
    }
    let Some(roles) = fields.next() else {
        return report.fail("end-of-line before role specification", None);
    };
    let mut roles = roles.clone();
    for token in &mut roles {
        if let Err(text) = token.compile() {
            return report.fail(&text, None);
        }
    }
    let address = if kind == Kind::Local {
        Address::None
    } else {
        let Some(tokens) = fields.next() else {
            return report.fail("end-of-line before IP address specification", None);
        };
        if tokens.len() > 1 {
            return report.fail(
                "multiple values specified for host address",
                Some("Specify one address range per line."),
            );
        }
        let token = &tokens[0];
        if token.keyword("all") {
            Address::All
        } else if token.keyword("samehost") {
            Address::SameHost
        } else if token.keyword("samenet") {
            Address::SameNet
        } else {
            let (host, bits) = match token.text.split_once('/') {
                Some((host, bits)) => (host, Some(bits)),
                None => (token.text.as_str(), None),
            };
            match (numeric_host(host), bits) {
                (None, Some(_)) => {
                    return report.fail(
                        &format!(
                            "specifying both host name and CIDR mask is invalid: \"{}\"",
                            token.text
                        ),
                        None,
                    );
                }
                (None, None) => Address::Name(host.to_owned()),
                (Some(ip), Some(bits)) => match cidr_mask(bits, ip.is_ipv4()) {
                    Some(mask) => Address::Mask(ip, mask),
                    None => {
                        return report.fail(
                            &format!("invalid CIDR mask in address \"{}\"", token.text),
                            None,
                        );
                    }
                },
                (Some(ip), None) => {
                    let Some(tokens) = fields.next() else {
                        return report.fail(
                            "end-of-line before netmask specification",
                            Some(
                                "Specify an address range in CIDR notation, or provide a separate netmask.",
                            ),
                        );
                    };
                    if tokens.len() > 1 {
                        return report.fail("multiple values specified for netmask", None);
                    }
                    let Some(mask) = numeric_host(&tokens[0].text) else {
                        return report.fail(
                            &format!("invalid IP mask \"{}\": {EAI_NONAME_TEXT}", tokens[0].text),
                            None,
                        );
                    };
                    if mask.is_ipv4() != ip.is_ipv4() {
                        return report.fail("IP address and mask do not match", None);
                    }
                    Address::Mask(ip, mask)
                }
            }
        }
    };
    let Some(tokens) = fields.next() else {
        return report.fail("end-of-line before authentication method", None);
    };
    if tokens.len() > 1 {
        return report.fail(
            "multiple values specified for authentication type",
            Some("Specify exactly one authentication type per line."),
        );
    }
    let name = tokens[0].text.as_str();
    let mut method = match name {
        "trust" => Method::Trust,
        "ident" => Method::Ident,
        "peer" => Method::Peer,
        "password" => Method::Password,
        "reject" => Method::Reject,
        "md5" => Method::Md5,
        "scram-sha-256" => Method::Scram,
        "cert" => Method::Cert,
        "oauth" => Method::OAuth,
        _ if OTHER_BUILDS.contains(&name) => {
            return report.fail(
                &format!("invalid authentication method \"{name}\": not supported by this build"),
                None,
            );
        }
        _ => return report.fail(&format!("invalid authentication method \"{name}\""), None),
    };
    if kind == Kind::Local && method == Method::Ident {
        method = Method::Peer;
    }
    if kind != Kind::Local && method == Method::Peer {
        return report.fail("peer authentication is only supported on local sockets", None);
    }
    if kind != Kind::HostSsl && method == Method::Cert {
        return report.fail("cert authentication is only supported on hostssl connections", None);
    }
    let mut parsed = HbaLine {
        file: line.file.clone(),
        number: line.number,
        raw: line.raw.clone(),
        kind,
        databases,
        roles,
        address,
        method,
        map: None,
        clientcert: ClientCert::Off,
        clientname: ClientName::Cn,
    };
    let mut oauth = OAuthOptions::default();
    // The options are the tokens of the rest of the fields, not one column.
    for token in fields.flatten() {
        let Some((name, value)) = token.text.split_once('=') else {
            return report.fail(
                &format!("authentication option not in name=value format: {}", token.text),
                None,
            );
        };
        if let Err(error) = parse_option(name, value, &mut parsed, &mut oauth) {
            return report.option(error);
        }
    }
    if method == Method::Cert {
        parsed.clientcert = ClientCert::VerifyFull;
    }
    if method == Method::OAuth {
        for (set, option) in [(oauth.scope, "scope"), (oauth.issuer, "issuer")] {
            if !set {
                return report.fail(
                    &format!(
                        "authentication method \"oauth\" requires argument \"{option}\" to be set"
                    ),
                    None,
                );
            }
        }
        return report.fail(
            "parameter \"oauth_validator_libraries\" must be set for authentication method \"oauth\"",
            None,
        );
    }
    Some(parsed)
}

/// The `oauth` options that the checks after the options look at.
#[derive(Debug, Default)]
struct OAuthOptions {
    scope: bool,
    issuer: bool,
}

/// The error of an option: the text for the log, and the text that PostgreSQL keeps for the view `pg_hba_file_rules`. For some errors PostgreSQL keeps no text, or a text that is not the same as the text of the log.
#[derive(Debug)]
struct OptionError {
    text: String,
    view: Option<String>,
}

impl OptionError {
    /// An error that PostgreSQL only writes to the log. The view shows the line with no error and with null values.
    fn quiet(text: String) -> OptionError {
        OptionError { text, view: None }
    }
}

impl From<String> for OptionError {
    fn from(text: String) -> OptionError {
        OptionError { view: Some(text.clone()), text }
    }
}

/// `parse_hba_auth_opt`.
fn parse_option(
    name: &str,
    value: &str,
    line: &mut HbaLine,
    oauth: &mut OAuthOptions,
) -> Result<(), OptionError> {
    let only = |methods: &str| {
        Err(OptionError::from(format!(
            "authentication option \"{name}\" is only valid for authentication methods {methods}"
        )))
    };
    match name {
        "map" => {
            if !matches!(line.method, Method::Ident | Method::Peer | Method::Cert | Method::OAuth) {
                return only("ident, peer, gssapi, sspi, cert, and oauth");
            }
            line.map = Some(value.to_owned());
        }
        "clientcert" => {
            if line.kind != Kind::HostSsl {
                return Err("clientcert can only be configured for \"hostssl\" rows"
                    .to_owned()
                    .into());
            }
            line.clientcert = match value {
                "verify-full" => ClientCert::VerifyFull,
                "verify-ca" if line.method == Method::Cert => {
                    return Err(OptionError {
                        text: "clientcert only accepts \"verify-full\" when using \"cert\" authentication"
                            .to_owned(),
                        view: Some(
                            "clientcert can only be set to \"verify-full\" when using \"cert\" authentication"
                                .to_owned(),
                        ),
                    });
                }
                "verify-ca" => ClientCert::VerifyCa,
                _ => {
                    return Err(OptionError::quiet(format!(
                        "invalid value for clientcert: \"{value}\""
                    )));
                }
            };
        }
        "clientname" => {
            if line.kind != Kind::HostSsl {
                return Err("clientname can only be configured for \"hostssl\" rows"
                    .to_owned()
                    .into());
            }
            line.clientname = match value {
                "CN" => ClientName::Cn,
                "DN" => ClientName::Dn,
                _ => {
                    return Err(OptionError::quiet(format!(
                        "invalid value for clientname: \"{value}\""
                    )));
                }
            };
        }
        "issuer" | "scope" | "validator" | "delegate_ident_mapping" => {
            if line.method != Method::OAuth {
                return only("oauth");
            }
            oauth.issuer |= name == "issuer";
            oauth.scope |= name == "scope";
        }
        _ if name.starts_with("validator.") => {
            if line.method != Method::OAuth {
                return only("oauth");
            }
            let key = &name["validator.".len()..];
            if key.is_empty()
                || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err(OptionError::quiet(format!(
                    "invalid OAuth validator option name: \"{name}\""
                )));
            }
        }
        _ => match OTHER_OPTIONS.iter().find(|(option, _)| *option == name) {
            // RADIUS left PostgreSQL in version 18, so its options are not known any more.
            Some((_, methods)) if *methods != "radius" => return only(methods),
            _ => {
                return Err(format!("unrecognized authentication option name: \"{name}\"").into());
            }
        },
    }
    Ok(())
}

/// The address of a numeric host, as `getaddrinfo` with `AI_NUMERICHOST` reads it: IPv6, or IPv4 in the forms of `inet_aton`, which also has `127.1` and hexadecimal parts.
fn numeric_host(text: &str) -> Option<IpAddr> {
    if text.contains(':') {
        return text.parse::<Ipv6Addr>().ok().map(IpAddr::V6);
    }
    let parts: Vec<&str> = text.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let mut values = Vec::with_capacity(parts.len());
    for part in &parts {
        let (digits, radix) =
            if let Some(hex) = part.strip_prefix("0x").or_else(|| part.strip_prefix("0X")) {
                (hex, 16)
            } else if part.len() > 1 && part.starts_with('0') {
                (&part[1..], 8)
            } else {
                (*part, 10)
            };
        if digits.is_empty() && radix != 16 {
            return None;
        }
        let value = if digits.is_empty() { 0 } else { u32::from_str_radix(digits, radix).ok()? };
        values.push(value);
    }
    let (last, head) = values.split_last()?;
    if head.iter().any(|&v| v > 255) {
        return None;
    }
    let room = 32 - 8 * u32::try_from(head.len()).ok()?;
    if room < 32 && *last >= 1 << room {
        return None;
    }
    let mut address = 0u32;
    for (at, value) in head.iter().enumerate() {
        address |= value << (24 - 8 * at);
    }
    address |= last;
    Some(IpAddr::V4(Ipv4Addr::from(address)))
}

/// `pg_sockaddr_cidr_mask`: the mask of `bits` leading ones, or `None` for bits that are not a number in the range of the family.
fn cidr_mask(bits: &str, v4: bool) -> Option<IpAddr> {
    if bits.is_empty() {
        return None;
    }
    let (negative, digits) = match bits.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, bits.strip_prefix('+').unwrap_or(bits)),
    };
    let digits = digits.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let bits: u32 = digits.parse().ok()?;
    if negative && bits != 0 {
        return None;
    }
    if v4 {
        (bits <= 32).then(|| {
            IpAddr::V4(Ipv4Addr::from(if bits == 0 { 0 } else { u32::MAX << (32 - bits) }))
        })
    } else {
        (bits <= 128).then(|| {
            IpAddr::V6(Ipv6Addr::from(if bits == 0 { 0 } else { u128::MAX << (128 - bits) }))
        })
    }
}

/// `check_ip`: the address is in the range of the address and the mask.
fn in_range(client: IpAddr, address: IpAddr, mask: IpAddr) -> bool {
    match (client, address, mask) {
        (IpAddr::V4(c), IpAddr::V4(a), IpAddr::V4(m)) => {
            (u32::from(c) ^ u32::from(a)) & u32::from(m) == 0
        }
        (IpAddr::V6(c), IpAddr::V6(a), IpAddr::V6(m)) => {
            (u128::from(c) ^ u128::from(a)) & u128::from(m) == 0
        }
        _ => false,
    }
}

/// What the server knows about the host name of a client, `remote_hostname_resolv`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Resolved {
    /// No line asked yet.
    Unknown,
    /// The name of the address, and whether the name gives the address back: `None` before the check.
    Name(String, Option<bool>),
    /// The reverse lookup failed with this text.
    NoName(String),
    /// The forward lookup of the name failed with this text.
    NoAddress(String, String),
}

/// A connection that wants to log in.
#[derive(Debug)]
pub(crate) struct Client<'a> {
    /// The address of the client, `None` on a Unix socket.
    pub(crate) address: Option<IpAddr>,
    /// The connection uses TLS.
    pub(crate) ssl: bool,
    pub(crate) user: &'a str,
    pub(crate) database: &'a str,
    resolved: RefCell<Resolved>,
}

impl<'a> Client<'a> {
    pub(crate) fn new(
        address: Option<IpAddr>,
        ssl: bool,
        user: &'a str,
        database: &'a str,
    ) -> Client<'a> {
        Client { address, ssl, user, database, resolved: RefCell::new(Resolved::Unknown) }
    }

    /// The text for the host in an error: the address, or `[local]` on a Unix socket.
    pub(crate) fn host(&self) -> String {
        self.address.map_or_else(|| "[local]".to_owned(), |ip| ip.to_string())
    }

    /// The `DETAIL` for the log of a connection that no line matched, from the host name lookups, `HOSTNAME_LOOKUP_DETAIL`.
    pub(crate) fn lookup_detail(&self) -> Option<String> {
        match &*self.resolved.borrow() {
            Resolved::Unknown => None,
            Resolved::Name(name, Some(true)) => {
                Some(format!("Client IP address resolved to \"{name}\", forward lookup matches."))
            }
            Resolved::Name(name, None) => Some(format!(
                "Client IP address resolved to \"{name}\", forward lookup not checked."
            )),
            Resolved::Name(name, Some(false)) => Some(format!(
                "Client IP address resolved to \"{name}\", forward lookup does not match."
            )),
            Resolved::NoName(error) => {
                Some(format!("Could not resolve client IP address to a host name: {error}."))
            }
            Resolved::NoAddress(name, error) => Some(format!(
                "Could not translate client host name \"{name}\" to IP address: {error}."
            )),
        }
    }

    /// `check_hostname`.
    fn host_matches(&self, net: &dyn Net, ip: IpAddr, pattern: &str) -> bool {
        let mut resolved = self.resolved.borrow_mut();
        if *resolved == Resolved::Unknown {
            *resolved = match net.host_name(ip) {
                Ok(name) => Resolved::Name(name, None),
                Err(error) => Resolved::NoName(error.message().to_owned()),
            };
        }
        let Resolved::Name(name, checked) = &*resolved else {
            return false;
        };
        if *checked == Some(false) {
            return false;
        }
        let matches = match pattern.strip_prefix('.') {
            Some(_) => {
                name.len() >= pattern.len()
                    && name.is_char_boundary(name.len() - pattern.len())
                    && name[name.len() - pattern.len()..].eq_ignore_ascii_case(pattern)
            }
            None => name.eq_ignore_ascii_case(pattern),
        };
        if !matches {
            return false;
        }
        if *checked == Some(true) {
            return true;
        }
        let name = name.clone();
        match net.host_addresses(&name) {
            Ok(addresses) => {
                let found = addresses.contains(&ip);
                *resolved = Resolved::Name(name, Some(found));
                found
            }
            Err(error) => {
                *resolved = Resolved::NoAddress(name, error.message().to_owned());
                false
            }
        }
    }
}

/// The loaded host rules.
#[derive(Debug, Clone)]
pub(crate) struct Hba {
    pub(crate) lines: Vec<HbaLine>,
}

impl Hba {
    /// `load_hba`. The messages for the log go to `log`. `None` means that the rules have an error or no entry, and the caller stops the start.
    pub(crate) fn load(
        io: &dyn Io,
        source: Source<'_>,
        ssl: bool,
        log: &mut Vec<String>,
    ) -> Option<Hba> {
        let lines = tokenize(io, source, log)?;
        let mut ok = true;
        let mut parsed = Vec::new();
        for line in &lines {
            if line.error.is_some() {
                ok = false;
                continue;
            }
            match parse_hba_line(line, ssl, &mut Report::new(line, log)) {
                Some(line) => parsed.push(line),
                None => ok = false,
            }
        }
        if ok && parsed.is_empty() {
            let name = match source {
                Source::File(name) | Source::Text { name, .. } => name,
            };
            log.push(format!("configuration file \"{name}\" contains no entries"));
            ok = false;
        }
        ok.then_some(Hba { lines: parsed })
    }

    /// `check_hba`: the first line that matches the client, or `None` for the implicit reject. The messages for the log go to `log`.
    pub(crate) fn find(
        &self,
        client: &Client<'_>,
        roles: &Roles,
        net: &dyn Net,
        log: &mut Vec<String>,
    ) -> Option<&HbaLine> {
        let mut interfaces: Option<Vec<(IpAddr, IpAddr)>> = None;
        self.lines.iter().find(|line| {
            match (line.kind, client.address) {
                (Kind::Local, None) => {}
                (Kind::Local, Some(_)) | (_, None) => return false,
                (kind, Some(ip)) => {
                    let skip = match kind {
                        Kind::HostNoSsl => client.ssl,
                        Kind::HostSsl => !client.ssl,
                        Kind::HostGssEnc => true,
                        _ => false,
                    };
                    if skip {
                        return false;
                    }
                    let matches = match &line.address {
                        Address::All | Address::None => true,
                        Address::Mask(address, mask) => in_range(ip, *address, *mask),
                        Address::Name(name) => client.host_matches(net, ip, name),
                        Address::SameHost | Address::SameNet => {
                            let list = interfaces.get_or_insert_with(|| {
                                net.interfaces().unwrap_or_else(|error| {
                                    log.push(format!(
                                        "error enumerating network interfaces: {}",
                                        error.message()
                                    ));
                                    Vec::new()
                                })
                            });
                            let same_host = line.address == Address::SameHost;
                            list.iter().any(|(address, mask)| {
                                if same_host {
                                    *address == ip
                                } else {
                                    in_range(ip, *address, *mask)
                                }
                            })
                        }
                    };
                    if !matches {
                        return false;
                    }
                }
            }
            check_db(client.database, client.user, &line.databases, roles)
                && check_role(client.user, &line.roles, roles)
        })
    }
}

/// The lines of a file for a view. The view reads the file again. A file that does not open is an error of the query, as `open_auth_file` with the level `ERROR` makes it, and an error in a line goes only to the row of the line.
fn view_lines(io: &dyn Io, source: Source<'_>) -> Result<Vec<TokenLine>> {
    let (name, text) = match source {
        Source::File(path) => {
            (path, String::from_utf8_lossy(&io.read_file(Path::new(path))?).into_owned())
        }
        Source::Text { name, text } => (name, text.to_owned()),
    };
    let mut log = Vec::new();
    Ok(tokenize(io, Source::Text { name, text: &text }, &mut log).unwrap_or_default())
}

/// A line number in a column of `int4`.
fn line_number(number: usize) -> i32 {
    i32::try_from(number).unwrap_or(i32::MAX)
}

/// `fill_hba_view`: each line of the host rules with its error. A line with an error has no rule number. `ssl` is the setting `ssl`.
///
/// # Errors
///
/// The error of a file that does not open.
pub(crate) fn hba_rules(io: &dyn Io, source: Source<'_>, ssl: bool) -> Result<Vec<HbaRule>> {
    let mut log = Vec::new();
    let mut number = 0;
    let mut rows = Vec::new();
    for line in view_lines(io, source)? {
        let (parsed, error) = match &line.error {
            Some(error) => (None, Some(error.clone())),
            None => {
                let mut report = Report::new(&line, &mut log);
                let parsed = parse_hba_line(&line, ssl, &mut report);
                (parsed, report.error)
            }
        };
        if error.is_none() {
            number += 1;
        }
        rows.push(HbaRule {
            rule_number: error.is_none().then_some(number),
            file_name: line.file,
            line_number: line_number(line.number),
            fields: parsed.map(|parsed| parsed.fields()),
            error,
        });
    }
    Ok(rows)
}

/// `fill_ident_view`: each line of the user maps with its error. A line with an error has no map number.
///
/// # Errors
///
/// The error of a file that does not open.
pub(crate) fn ident_mappings(io: &dyn Io, source: Source<'_>) -> Result<Vec<IdentMapping>> {
    let mut log = Vec::new();
    let mut number = 0;
    let mut rows = Vec::new();
    for line in view_lines(io, source)? {
        let (parsed, error) = match &line.error {
            Some(error) => (None, Some(error.clone())),
            None => {
                let mut report = Report::new(&line, &mut log);
                let parsed = parse_ident_line(&line, &mut report);
                (parsed, report.error)
            }
        };
        if error.is_none() {
            number += 1;
        }
        rows.push(IdentMapping {
            map_number: error.is_none().then_some(number),
            file_name: line.file,
            line_number: line_number(line.number),
            fields: parsed.map(|parsed| IdentFields {
                map_name: parsed.map,
                sys_name: parsed.system_user.text,
                pg_username: parsed.pg_user.text,
            }),
            error,
        });
    }
    Ok(rows)
}

/// `check_role`.
fn check_role(user: &str, tokens: &[Token], roles: &Roles) -> bool {
    tokens.iter().any(|token| {
        if let Some(group) = token.group() {
            roles.is_member(user, group)
        } else if token.keyword("all") {
            true
        } else if token.regex {
            token.finds(user)
        } else {
            token.text == user
        }
    })
}

/// `check_db`.
fn check_db(database: &str, user: &str, tokens: &[Token], roles: &Roles) -> bool {
    tokens.iter().any(|token| {
        if token.keyword("all") {
            true
        } else if token.keyword("sameuser") {
            database == user
        } else if token.keyword("samegroup") || token.keyword("samerole") {
            roles.is_member(user, database)
        } else if token.keyword("replication") {
            false
        } else if token.regex {
            token.finds(database)
        } else {
            token.text == database
        }
    })
}

/// One line of the user maps, `IdentLine`.
#[derive(Debug, Clone)]
struct IdentLine {
    map: String,
    system_user: Token,
    pg_user: Token,
}

/// The loaded user maps.
#[derive(Debug, Clone, Default)]
pub(crate) struct Ident {
    lines: Vec<IdentLine>,
}

impl Ident {
    /// `load_ident`. `None` means that the maps have an error, which is in `log`.
    pub(crate) fn load(io: &dyn Io, source: Source<'_>, log: &mut Vec<String>) -> Option<Ident> {
        let lines = tokenize(io, source, log)?;
        let mut ok = true;
        let mut parsed = Vec::new();
        for line in &lines {
            if line.error.is_some() {
                ok = false;
                continue;
            }
            match parse_ident_line(line, &mut Report::new(line, log)) {
                Some(line) => parsed.push(line),
                None => ok = false,
            }
        }
        ok.then_some(Ident { lines: parsed })
    }

    /// `check_usermap`: the system user `system_user` may log in as `pg_user` with the map `map`. A failure writes the reason to `log`.
    pub(crate) fn check(
        &self,
        map: Option<&str>,
        pg_user: &str,
        system_user: &str,
        roles: &Roles,
        log: &mut Vec<String>,
    ) -> bool {
        let Some(map) = map.filter(|map| !map.is_empty()) else {
            if pg_user == system_user {
                return true;
            }
            log.push(format!(
                "provided user name ({pg_user}) and authenticated user name ({system_user}) do not match"
            ));
            return false;
        };
        let mut found = false;
        for line in self.lines.iter().filter(|line| line.map == map) {
            match line.check(pg_user, system_user, roles) {
                Ok(true) => found = true,
                Ok(false) => continue,
                Err(text) => {
                    log.push(text);
                    return false;
                }
            }
            break;
        }
        if !found {
            log.push(format!(
                "no match in usermap \"{map}\" for user \"{pg_user}\" authenticated as \"{system_user}\""
            ));
        }
        found
    }
}

impl IdentLine {
    /// `check_ident_usermap` for a line of the map: the line lets `system_user` log in as `pg_user`. The error is the message for the log, and it stops the search.
    fn check(&self, pg_user: &str, system_user: &str, roles: &Roles) -> Result<bool, String> {
        let Some(pattern) = self.system_user.pattern() else {
            return Ok(self.system_user.text == system_user
                && check_role(pg_user, std::slice::from_ref(&self.pg_user), roles));
        };
        let found = match auth_search(pattern, system_user) {
            Ok(Some(found)) => found,
            Ok(None) => return Ok(false),
            Err(text) => {
                return Err(format!("regular expression match for \"{pattern}\" failed: {text}"));
            }
        };
        let target = &self.pg_user;
        if target.group().is_some() || target.regex || !target.text.contains("\\1") {
            return Ok(check_role(pg_user, std::slice::from_ref(target), roles));
        }
        let Some(group) = found.group else {
            return Err(format!(
                "regular expression \"{pattern}\" has no subexpressions as requested by backreference in \"{}\"",
                target.text
            ));
        };
        // The new name is quoted, so it is never a keyword or a group.
        let expanded = Token::new(target.text.replace("\\1", &group), true);
        Ok(check_role(pg_user, std::slice::from_ref(&expanded), roles))
    }
}

/// `parse_ident_line`. An error goes to the log of `report` and gives `None`.
fn parse_ident_line(line: &TokenLine, report: &mut Report<'_>) -> Option<IdentLine> {
    let mut fields = line.fields.iter();
    let mut tokens = Vec::with_capacity(3);
    for _ in 0..3 {
        let Some(field) = fields.next() else {
            return report.fail("missing entry at end of line", None);
        };
        if field.len() > 1 {
            return report.fail("multiple values in ident field", None);
        }
        tokens.push(field[0].clone());
    }
    let mut pg_user = tokens.pop()?;
    let mut system_user = tokens.pop()?;
    let map = tokens.pop()?.text;
    for token in [&mut system_user, &mut pg_user] {
        if let Err(text) = token.compile() {
            return report.fail(&text, None);
        }
    }
    Some(IdentLine { map, system_user, pg_user })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rupg_platform::OpenMode;
    use rupg_platform::sim::{SimIo, SimNet};

    use super::*;

    fn tokens(line: &str) -> Vec<Vec<(String, bool)>> {
        let io = SimIo::new(0);
        let mut log = Vec::new();
        let mut tokenizer = Tokenizer { io: &io, log: &mut log, stack: Vec::new() };
        let mut lines = Vec::new();
        tokenizer.file("/x/pg_hba.conf", line, &mut lines, 0);
        lines
            .into_iter()
            .flat_map(|line| line.fields)
            .map(|field| field.into_iter().map(|t| (t.text, t.quoted)).collect())
            .collect()
    }

    fn plain(fields: &[&[&str]]) -> Vec<Vec<(String, bool)>> {
        fields
            .iter()
            .map(|field| field.iter().map(|t| ((*t).to_owned(), false)).collect())
            .collect()
    }

    #[test]
    fn the_tokens_of_a_line() {
        assert_eq!(tokens("host all a,b 1.2.3.4/32 md5"), {
            plain(&[&["host"], &["all"], &["a", "b"], &["1.2.3.4/32"], &["md5"]])
        });
        // A space before the comma ends the field, as in PostgreSQL.
        assert_eq!(tokens("a ,b"), plain(&[&["a"], &["b"]]));
        assert_eq!(tokens("a, b # c d"), plain(&[&["a", "b"]]));
        assert_eq!(tokens("a#b c"), plain(&[&["a"]]));
        assert_eq!(
            tokens("\"a b\" \"x\"\"y\" \"\""),
            vec![
                vec![("a b".to_owned(), true)],
                vec![("x\"y".to_owned(), true)],
                vec![(String::new(), true)],
            ]
        );
        assert_eq!(tokens("local \\\n all"), plain(&[&["local"], &["all"]]));
        assert_eq!(tokens("a\r\nb\n"), plain(&[&["a"], &["b"]]));
    }

    fn write(io: &SimIo, path: &str, text: &str) {
        let file = io.open(Path::new(path), OpenMode::Create).unwrap();
        file.write_at(0, text.as_bytes()).unwrap();
    }

    fn load(text: &str) -> (Option<Hba>, Vec<String>) {
        let io = SimIo::new(0);
        write(&io, "/d/pg_hba.conf", text);
        let mut log = Vec::new();
        let hba = Hba::load(&io, Source::File("/d/pg_hba.conf"), false, &mut log);
        (hba, log)
    }

    #[test]
    fn the_errors_of_postgresql() {
        let cases = [
            ("foo all all trust", "invalid connection type \"foo\""),
            ("local,host all all trust", "multiple values specified for connection type"),
            ("local all", "end-of-line before role specification"),
            ("host all all", "end-of-line before IP address specification"),
            ("host all all 1.2.3.4", "end-of-line before netmask specification"),
            ("host all all 1.2.3.4 255.0.0.0", "end-of-line before authentication method"),
            ("host all all 1.2.3.4 x trust", "invalid IP mask \"x\": Name or service not known"),
            ("host all all 1.2.3.4/33 trust", "invalid CIDR mask in address \"1.2.3.4/33\""),
            (
                "host all all foo/8 trust",
                "specifying both host name and CIDR mask is invalid: \"foo/8\"",
            ),
            ("host all all ::1 255.0.0.0 trust", "IP address and mask do not match"),
            ("local all all foo", "invalid authentication method \"foo\""),
            (
                "local all all ldap",
                "invalid authentication method \"ldap\": not supported by this build",
            ),
            ("host all all all peer", "peer authentication is only supported on local sockets"),
            (
                "host all all all cert",
                "cert authentication is only supported on hostssl connections",
            ),
            ("local all all trust x", "authentication option not in name=value format: x"),
            (
                "local all all trust map=x",
                "authentication option \"map\" is only valid for authentication methods ident, peer, gssapi, sspi, cert, and oauth",
            ),
            (
                "local all all trust ldapserver=x",
                "authentication option \"ldapserver\" is only valid for authentication methods ldap",
            ),
            (
                "local all all trust clientcert=verify-ca",
                "clientcert can only be configured for \"hostssl\" rows",
            ),
            ("local all all trust foo=1", "unrecognized authentication option name: \"foo\""),
            (
                "local all all oauth scope=a",
                "authentication method \"oauth\" requires argument \"issuer\" to be set",
            ),
            (
                "local all all oauth scope=a issuer=b",
                "parameter \"oauth_validator_libraries\" must be set for authentication method \"oauth\"",
            ),
            ("local all /( trust", "invalid regular expression \"(\": parentheses () not balanced"),
            (
                "local \"/a[\" all trust",
                "invalid regular expression \"a[\": brackets [] not balanced",
            ),
        ];
        for (line, error) in cases {
            let (hba, log) = load(line);
            assert!(hba.is_none(), "{line}");
            let first = log[0].lines().next().unwrap();
            assert_eq!(first, error, "{line}");
            assert!(
                log[0].ends_with("CONTEXT:  line 1 of configuration file \"/d/pg_hba.conf\""),
                "{line}"
            );
        }
        let (hba, log) = load("# nothing\n\n");
        assert!(hba.is_none());
        assert_eq!(log, ["configuration file \"/d/pg_hba.conf\" contains no entries"]);
        let (hba, log) = load("hostssl all all all trust\n");
        assert!(hba.is_some());
        assert_eq!(
            log,
            [
                "hostssl record cannot match because SSL is disabled\nHINT:  Set \"ssl = on\" in postgresql.conf.\nCONTEXT:  line 1 of configuration file \"/d/pg_hba.conf\""
            ]
        );
        // Every bad line goes to the log, not only the first.
        let (hba, log) = load("foo\nlocal all all trust\nbar\n");
        assert!(hba.is_none());
        assert_eq!(log.len(), 2);
        assert!(log[1].ends_with("line 3 of configuration file \"/d/pg_hba.conf\""));
        let io = SimIo::new(0);
        let mut log = Vec::new();
        assert!(Hba::load(&io, Source::File("/none"), false, &mut log).is_none());
        assert!(log[0].starts_with("could not open file \"/none\": "), "{log:?}");
    }

    #[test]
    fn stored_text() {
        let io = SimIo::new(0);
        let mut log = Vec::new();
        let source = Source::Text { name: "pg_hba.conf", text: "local all all trust\nx\n" };
        assert!(Hba::load(&io, source, false, &mut log).is_none());
        assert_eq!(
            log,
            [
                "invalid connection type \"x\"\nCONTEXT:  line 2 of configuration file \"pg_hba.conf\""
            ]
        );
    }

    #[test]
    fn addresses() {
        assert_eq!(numeric_host("127.1"), Some("127.0.0.1".parse().unwrap()));
        assert_eq!(numeric_host("0x7f.1"), Some("127.0.0.1".parse().unwrap()));
        assert_eq!(numeric_host("10.0.0.256"), None);
        assert_eq!(numeric_host("localhost"), None);
        assert_eq!(numeric_host("::1"), Some("::1".parse().unwrap()));
        let mask = cidr_mask("8", true).unwrap();
        assert!(in_range("10.1.2.3".parse().unwrap(), "10.0.0.0".parse().unwrap(), mask));
        assert!(!in_range("11.1.2.3".parse().unwrap(), "10.0.0.0".parse().unwrap(), mask));
        assert!(!in_range("::1".parse().unwrap(), "10.0.0.0".parse().unwrap(), mask));
        assert_eq!(cidr_mask("", true), None);
        assert_eq!(cidr_mask("1x", true), None);
        assert_eq!(cidr_mask("129", false), None);
    }

    fn roles() -> Roles {
        Roles { superuser: "postgres".into() }
    }

    #[test]
    fn the_first_line_that_matches() {
        let (hba, _) = load(
            "local sameuser all trust\n\
             local all +postgres md5\n\
             host all bob ::1/128 password\n\
             hostssl all all 127.0.0.1/32 scram-sha-256\n\
             host \"all\" all 127.0.0.0 255.0.0.0 reject\n\
             host samerole all 127.0.0.1/32 trust\n\
             host all all 127.0.0.1/32 md5\n",
        );
        let hba = hba.unwrap();
        let roles = roles();
        let net = SimNet::new();
        let find = |address: Option<&str>, ssl, user, database| {
            let client = Client::new(address.map(|a| a.parse().unwrap()), ssl, user, database);
            hba.find(&client, &roles, &net, &mut Vec::new()).map(|line| line.number)
        };
        assert_eq!(find(None, false, "alice", "alice"), Some(1));
        assert_eq!(find(None, false, "postgres", "db"), Some(2));
        assert_eq!(find(None, false, "alice", "db"), None);
        assert_eq!(find(Some("::1"), false, "bob", "db"), Some(3));
        assert_eq!(find(Some("::1"), false, "alice", "db"), None);
        assert_eq!(find(Some("127.0.0.1"), true, "alice", "db"), Some(4));
        // A quoted "all" is a database name.
        assert_eq!(find(Some("127.0.0.1"), false, "alice", "all"), Some(5));
        assert_eq!(find(Some("127.0.0.1"), false, "postgres", "postgres"), Some(6));
        assert_eq!(find(Some("127.0.0.1"), false, "postgres", "db"), Some(7));
    }

    #[test]
    fn host_names() {
        let (hba, _) = load("host all all .example.com trust\nhost all all samehost trust\n");
        let hba = hba.unwrap();
        let roles = roles();
        let net = SimNet::new();
        let client = Client::new(Some("10.0.0.1".parse().unwrap()), false, "postgres", "postgres");
        let mut log = Vec::new();
        assert!(hba.find(&client, &roles, &net, &mut log).is_none());
        assert_eq!(
            client.lookup_detail().as_deref(),
            Some("Could not resolve client IP address to a host name: Name or service not known.")
        );
        assert!(log.is_empty());
    }

    #[test]
    fn ident_maps() {
        let io = SimIo::new(0);
        let mut log = Vec::new();
        let source =
            Source::Text { name: "pg_ident.conf", text: "m tom postgres\nm ann +postgres\n" };
        let ident = Ident::load(&io, source, &mut log).unwrap();
        let roles = roles();
        let mut log = Vec::new();
        assert!(ident.check(Some("m"), "postgres", "tom", &roles, &mut log));
        assert!(ident.check(Some("m"), "postgres", "ann", &roles, &mut log));
        assert!(ident.check(None, "postgres", "postgres", &roles, &mut log));
        assert!(log.is_empty());
        assert!(!ident.check(Some("m"), "alice", "tom", &roles, &mut log));
        assert!(!ident.check(None, "postgres", "tom", &roles, &mut log));
        assert_eq!(
            log,
            [
                "no match in usermap \"m\" for user \"alice\" authenticated as \"tom\"",
                "provided user name (postgres) and authenticated user name (tom) do not match",
            ]
        );
        let mut log = Vec::new();
        let source = Source::Text { name: "pg_ident.conf", text: "m a\nm a,b c\nm /^x( c\n" };
        assert!(Ident::load(&io, source, &mut log).is_none());
        let first: Vec<&str> = log.iter().map(|l| l.lines().next().unwrap()).collect();
        assert_eq!(
            first,
            [
                "missing entry at end of line",
                "multiple values in ident field",
                "invalid regular expression \"^x(\": parentheses () not balanced",
            ]
        );
    }

    #[test]
    fn regular_expressions() {
        let (hba, _) = load(
            "local /^db[0-9]$ all reject\n\
             local all /^post trust\n\
             local \"/gres$\" all md5\n",
        );
        let hba = hba.unwrap();
        let roles = roles();
        let net = SimNet::new();
        let find = |user, database| {
            let client = Client::new(None, false, user, database);
            hba.find(&client, &roles, &net, &mut Vec::new()).map(|line| line.number)
        };
        assert_eq!(find("postgres", "db1"), Some(1));
        assert_eq!(find("postgres", "db12"), Some(2));
        assert_eq!(find("alice", "ingres"), Some(3));
        assert_eq!(find("alice", "db"), None);
    }

    #[test]
    fn ident_regular_expressions() {
        let io = SimIo::new(0);
        let mut log = Vec::new();
        let text = "m /^(.*)@example\\.com$ \\1\n\
                    m /^admin \\1\n\
                    n /^a /^post\n\
                    o /^(.*)$ \\1\n\
                    p /^x(y)?z \\1\n";
        let ident =
            Ident::load(&io, Source::Text { name: "pg_ident.conf", text }, &mut log).unwrap();
        assert!(log.is_empty());
        let roles = roles();
        assert!(ident.check(Some("m"), "postgres", "postgres@example.com", &roles, &mut log));
        assert!(ident.check(Some("m"), "alice", "alice@example.com", &roles, &mut log));
        assert!(ident.check(Some("n"), "postgres", "ab", &roles, &mut log));
        assert!(log.is_empty());
        assert!(!ident.check(Some("m"), "postgres", "bob@example.com", &roles, &mut log));
        assert!(!ident.check(Some("m"), "postgres", "admin1", &roles, &mut log));
        // The name that the match makes is quoted, so "all" is not the keyword.
        assert!(!ident.check(Some("o"), "postgres", "all", &roles, &mut log));
        assert!(!ident.check(Some("p"), "x", "xz", &roles, &mut log));
        assert_eq!(
            log,
            [
                "no match in usermap \"m\" for user \"postgres\" authenticated as \"bob@example.com\"",
                "regular expression \"^admin\" has no subexpressions as requested by backreference in \"\\1\"",
                "no match in usermap \"o\" for user \"postgres\" authenticated as \"all\"",
                "regular expression \"^x(y)?z\" has no subexpressions as requested by backreference in \"\\1\"",
            ]
        );
    }

    #[test]
    fn includes() {
        let io = SimIo::new(0);
        write(&io, "/d/users", "alice\nbob, carol\n");
        write(&io, "/d/conf.d/b.conf", "local all all reject\n");
        write(&io, "/d/conf.d/a.conf", "local db all trust\n");
        write(&io, "/d/conf.d/.c.conf", "bad\n");
        write(
            &io,
            "/d/pg_hba.conf",
            "local all @users md5\ninclude_dir conf.d\ninclude_if_exists none\n",
        );
        let mut log = Vec::new();
        let hba = Hba::load(&io, Source::File("/d/pg_hba.conf"), false, &mut log).unwrap();
        let roles: Vec<&str> = hba.lines[0].roles.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(roles, ["alice", "bob", "carol"]);
        assert_eq!(hba.lines[1].method, Method::Trust);
        assert_eq!(hba.lines[2].method, Method::Reject);
        assert_eq!(hba.lines[1].file, "/d/conf.d/a.conf");
        // PostgreSQL logs the failed open first, then the skip.
        assert_eq!(log.len(), 2, "{log:?}");
        assert!(log[0].starts_with("could not open file \"/d/none\""), "{log:?}");
        assert!(log[1].starts_with("skipping missing authentication file \"/d/none\""), "{log:?}");
        assert!(log[1].ends_with("line 3 of configuration file \"/d/pg_hba.conf\""), "{log:?}");
    }

    #[test]
    fn paths() {
        assert_eq!(absolute_location("x", "/a/b/pg_hba.conf"), "/a/b/x");
        assert_eq!(absolute_location("../x", "/a/b/pg_hba.conf"), "/a/x");
        assert_eq!(absolute_location("/y", "/a/b/pg_hba.conf"), "/y");
        assert_eq!(canonical(&PathBuf::from("/a/./b/../c/")), "/a/c");
    }
}
