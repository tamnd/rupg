//! `ts_headline` of `wparser.c` for `text`: `hlparsetext` and `generateHeadline` of `ts_parse.c`, and `prsd_headline`, the headline function of the default parser in `wparser_def.c`. The function finds the shortest parts of the text that match the query, selects the best part or the best fragments, and puts the words of the query between `StartSel` and `StopSel`.

use rupg_analyze::tsdict::{self, Config};
use rupg_common::{Error, Result, SqlState};
use rupg_types::{QueryItem, TSVECTOR_MAX_WORD, Value, int4_in, limit_position};

use crate::tsmatch::{self, Phrase, Ternary};
use crate::tsparser::{
    ASCII_HWORD, DECIMAL, HWORD, NUM_HWORD, PROTOCOL, SCIENTIFIC, SIGNED_INT, SPACE, TAG,
    UNSIGNED_INT, URL, VERSION, XML_ENTITY,
};
use crate::{Call, Kernel, Session, bad_value, tsearch, tsparser, type_error};

/// `HeadlineWordEntry`: a token of the text with the operand of the query that it matches.
#[derive(Clone, Debug)]
struct Word<'t> {
    kind: u8,
    text: &'t str,
    /// The position of the lexemes of the token, or 0 for a token with no lexemes.
    pos: u16,
    /// The index of the operand in the query.
    item: Option<usize>,
    /// A copy of the token for one more operand that it matches.
    repeated: bool,
    selected: bool,
    /// `in`: the token is in the headline.
    shown: bool,
    replace: bool,
    skip: bool,
}

/// `TS_IDIGNORE`.
fn ignored(kind: u8) -> bool {
    matches!(kind, TAG | PROTOCOL | SPACE | XML_ENTITY)
}

/// `HLIDREPLACE`: a tag becomes a space.
fn replaced(kind: u8) -> bool {
    kind == TAG
}

/// `HLIDSKIP` and `XMLHLIDSKIP`: a URL and a word with hyphens are not in the headline, but their parts are.
fn skipped(kind: u8) -> bool {
    matches!(kind, URL | NUM_HWORD | ASCII_HWORD | HWORD)
}

/// `NONWORDTOKEN`: a token that does not count as a word.
fn non_word(kind: u8) -> bool {
    kind == SPACE || replaced(kind) || skipped(kind)
}

/// `NOENDTOKEN`: a token that a headline does not end with.
fn no_end(kind: u8) -> bool {
    non_word(kind)
        || matches!(kind, SCIENTIFIC | VERSION | DECIMAL | SIGNED_INT | UNSIGNED_INT)
        || ignored(kind)
}

/// `hlfinditem`: the position of the last token, and the operands of the query that one of its lexemes matches. The token gets the first operand, and a copy of the token gets each other operand.
fn find_items(words: &mut Vec<Word<'_>>, items: &[QueryItem], pos: i32, lexeme: &str) {
    let Some(at) = words.len().checked_sub(1) else { return };
    words[at].pos = limit_position(pos);
    for (i, item) in items.iter().enumerate() {
        let QueryItem::Operand { word, prefix, .. } = item else { continue };
        let found = if *prefix { lexeme.starts_with(word.as_str()) } else { lexeme == word };
        if !found {
            continue;
        }
        if words[at].item.is_some() {
            words.push(Word { item: Some(i), repeated: true, ..words[at].clone() });
        } else {
            words[at].item = Some(i);
        }
    }
}

/// `hlparsetext`: the tokens of the text, and for each token the operands that its lexemes match.
fn parse<'t>(
    session: &dyn Session,
    config: &Config,
    items: &[QueryItem],
    text: &'t str,
) -> Vec<Word<'t>> {
    let mut words = Vec::new();
    let mut pos = 0;
    for (kind, token) in tsparser::tokens(text) {
        if token.len() > TSVECTOR_MAX_WORD {
            session.warn(tsearch::too_long_word());
            continue;
        }
        words.push(Word {
            kind,
            text: token,
            pos: 0,
            item: None,
            repeated: false,
            selected: false,
            shown: false,
            replace: false,
            skip: false,
        });
        let Some(dicts) = config.map.get(usize::from(kind)) else { continue };
        let Some(lexemes) = dicts.iter().find_map(|dict| dict.lexize(token)) else { continue };
        pos += 1;
        for lexeme in lexemes {
            find_items(&mut words, items, pos, &lexeme);
        }
    }
    words
}

/// `checkcondition_HL`: the match of an operand with the tokens, with the positions of the tokens that match it.
fn check(words: &[Word<'_>], at: usize, mut data: Option<&mut Phrase>) -> Ternary {
    for word in words.iter().filter(|w| w.item == Some(at)) {
        let Some(data) = data.as_deref_mut() else { return Ternary::Yes };
        if data.pos.last().is_none_or(|last| *last < word.pos) {
            data.pos.push(word.pos);
        }
    }
    if data.is_some_and(|data| !data.pos.is_empty()) { Ternary::Yes } else { Ternary::No }
}

/// The options of the headline.
#[derive(Debug)]
struct Options {
    min_words: i32,
    max_words: i32,
    short_word: i32,
    max_fragments: i32,
    highlight_all: bool,
    start_sel: String,
    stop_sel: String,
    delimiter: String,
}

/// The error for a bad value of an option.
fn invalid(message: String) -> Error {
    Error::new(SqlState::INVALID_PARAMETER_VALUE, message)
}

/// The options of `prsd_headline` with their defaults, and the checks of their values.
fn options(text: Option<&str>) -> Result<Options> {
    let mut out = Options {
        min_words: 15,
        max_words: 35,
        short_word: 3,
        max_fragments: 0,
        highlight_all: false,
        start_sel: "<b>".to_string(),
        stop_sel: "</b>".to_string(),
        delimiter: " ... ".to_string(),
    };
    let list = match text {
        Some(text) => tsdict::deserialize(text)?,
        None => Vec::new(),
    };
    let int = |value: &str| int4_in(value).map_err(type_error);
    for (name, value) in list {
        match name.to_ascii_lowercase().as_str() {
            "maxwords" => out.max_words = int(&value)?,
            "minwords" => out.min_words = int(&value)?,
            "shortword" => out.short_word = int(&value)?,
            "maxfragments" => out.max_fragments = int(&value)?,
            "startsel" => out.start_sel = value,
            "stopsel" => out.stop_sel = value,
            "fragmentdelimiter" => out.delimiter = value,
            "highlightall" => {
                out.highlight_all = ["1", "on", "true", "t", "y", "yes"]
                    .iter()
                    .any(|v| value.eq_ignore_ascii_case(v));
            }
            _ => return Err(invalid(format!("unrecognized headline parameter: \"{name}\""))),
        }
    }
    if !out.highlight_all {
        if out.min_words >= out.max_words {
            return Err(invalid("MinWords must be less than MaxWords".to_string()));
        }
        if out.min_words <= 0 {
            return Err(invalid("MinWords must be positive".to_string()));
        }
        if out.short_word < 0 {
            return Err(invalid("ShortWord must be >= 0".to_string()));
        }
        if out.max_fragments < 0 {
            return Err(invalid("MaxFragments must be >= 0".to_string()));
        }
    }
    Ok(out)
}

/// `CoverPos`: a fragment of a cover for `mark_hl_fragments`.
#[derive(Debug)]
struct Fragment {
    start: i32,
    end: i32,
    /// The number of words that match the query.
    poslen: i32,
    /// The number of words.
    curlen: i32,
    chosen: bool,
    excluded: bool,
}

/// The tokens of the text for the selection of the headline. The indexes are `int` values as in PostgreSQL.
struct Headline<'t, 'q> {
    words: Vec<Word<'t>>,
    items: &'q [QueryItem],
    short_word: i32,
}

impl<'t> Headline<'t, '_> {
    fn len(&self) -> i32 {
        i32::try_from(self.words.len()).unwrap_or(i32::MAX)
    }

    fn word(&self, i: i32) -> Option<&Word<'_>> {
        usize::try_from(i).ok().and_then(|i| self.words.get(i))
    }

    /// True when the token is not a word.
    fn non_word(&self, i: i32) -> bool {
        self.word(i).is_some_and(|w| non_word(w.kind))
    }

    /// `INTERESTINGWORD`: a token that matches an operand and is not a copy.
    fn interesting(&self, i: i32) -> bool {
        self.word(i).is_some_and(|w| w.item.is_some() && !w.repeated)
    }

    /// `BADENDPOINT`: a token that is not a word or is a short word, unless it is interesting.
    fn bad_end(&self, i: i32) -> bool {
        self.word(i).is_some_and(|w| {
            let short =
                i64::try_from(w.text.len()).unwrap_or(i64::MAX) <= i64::from(self.short_word);
            (no_end(w.kind) || short) && !self.interesting(i)
        })
    }

    /// The token at an index that is in range.
    fn at(&mut self, i: i32) -> &mut Word<'t> {
        &mut self.words[usize::try_from(i).unwrap_or_default()]
    }

    /// `hlCover`: the first and the last index of the next shortest part of the text that matches the query, from the lexeme position `next`.
    fn cover(&self, locations: &[Phrase], next: &mut i32) -> Option<(i32, i32)> {
        let mut pos = *next;
        loop {
            // The greatest of the first positions of the parts at or after `pos`.
            let mut pose = -1;
            for data in locations {
                let first = data.pos.iter().map(|p| i32::from(*p)).find(|p| *p >= pos)?;
                pose = pose.max(first);
            }
            if pose < 0 {
                return None;
            }
            // The least of the last start positions of the parts at or before `pose`.
            let mut posb = i32::MAX - 1;
            for data in locations {
                let last = data
                    .pos
                    .iter()
                    .rev()
                    .map(|p| i32::from(*p) - data.width)
                    .find(|p| *p <= pose)
                    .unwrap_or(-1);
                posb = posb.min(last);
            }
            posb = posb.max(pos);
            if posb <= pose {
                let (mut idxb, mut idxe) = (-1, -1);
                for (i, word) in (0..).zip(&self.words) {
                    if word.item.is_none() {
                        continue;
                    }
                    if idxb < 0 && i32::from(word.pos) >= posb {
                        idxb = i;
                    }
                    if i32::from(word.pos) > pose {
                        break;
                    }
                    idxe = i;
                }
                if idxb >= 0 && idxe >= idxb {
                    let part = &self.words[idxb as usize..=idxe as usize];
                    if tsmatch::matches_with(self.items, &|at, data| check(part, at, data)) {
                        *next = posb + 1;
                        return Some((idxb, idxe));
                    }
                }
            }
            pos = posb + 1;
        }
    }

    /// `mark_fragment`: puts the tokens from `start` to `end` in the headline.
    fn mark(&mut self, all: bool, start: i32, end: i32) {
        for i in start..=end {
            let word = self.at(i);
            if word.item.is_some() {
                word.selected = true;
            }
            if !all && replaced(word.kind) {
                word.replace = true;
            } else if skipped(word.kind) {
                word.skip = true;
            }
            word.shown = !word.repeated;
        }
    }

    /// `get_next_fragment`: the next fragment of a cover with at most `max_words` words, which starts and ends with an interesting word when it can. The result is the number of words and the number of interesting words.
    fn next_fragment(&self, start: &mut i32, end: &mut i32, max_words: i32) -> (i32, i32) {
        let mut i = *start;
        while i <= *end {
            *start = i;
            if self.interesting(i) {
                break;
            }
            i += 1;
        }
        let (mut curlen, mut poslen) = (0, 0);
        i = *start;
        while i <= *end && curlen < max_words {
            if !self.non_word(i) {
                curlen += 1;
            }
            if self.interesting(i) {
                poslen += 1;
            }
            i += 1;
        }
        if *end > i {
            *end = i;
            let mut k = i;
            while k >= *start {
                *end = k;
                if self.interesting(k) {
                    break;
                }
                if !self.non_word(k) {
                    curlen -= 1;
                }
                k -= 1;
            }
        }
        (curlen, poslen)
    }

    /// `mark_hl_fragments`: the selector for `MaxFragments` greater than 0.
    fn mark_fragments(&mut self, locations: &[Phrase], opts: &Options) {
        let (all, max_words) = (opts.highlight_all, opts.max_words);
        let n = self.len();
        let mut covers: Vec<Fragment> = Vec::new();
        let mut next = 0;
        while let Some((p, q)) = self.cover(locations, &mut next) {
            let (mut start, mut end) = (p, q);
            while start <= end {
                let (curlen, poslen) = self.next_fragment(&mut start, &mut end, max_words);
                covers.push(Fragment {
                    start,
                    end,
                    poslen,
                    curlen,
                    chosen: false,
                    excluded: false,
                });
                start = end + 1;
                end = q;
            }
        }
        let mut count = 0;
        for _ in 0..opts.max_fragments {
            // The cover with the most interesting words, and then with the fewest words.
            let (mut most, mut fewest, mut best) = (0, i32::MAX, None);
            for (i, c) in covers.iter().enumerate() {
                if !c.chosen
                    && !c.excluded
                    && (most < c.poslen || (most == c.poslen && fewest > c.curlen))
                {
                    most = c.poslen;
                    fewest = c.curlen;
                    best = Some(i);
                }
            }
            let Some(best) = best else { break };
            covers[best].chosen = true;
            let (mut start, mut end, mut curlen) =
                (covers[best].start, covers[best].end, covers[best].curlen);
            if curlen < max_words {
                // Stretch the cover to each side by half of the free words.
                let max_stretch = (max_words - curlen) / 2;
                let mut stretch = 0;
                let mut marker = start;
                let mut i = start - 1;
                while i >= 0 && stretch < max_stretch && !self.word(i).is_some_and(|w| w.shown) {
                    if !self.non_word(i) {
                        curlen += 1;
                        stretch += 1;
                    }
                    marker = i;
                    i -= 1;
                }
                i = marker;
                while i < start && self.bad_end(i) {
                    if !self.non_word(i) {
                        curlen -= 1;
                    }
                    i += 1;
                }
                start = i;
                marker = end;
                i = end + 1;
                while i < n && curlen < max_words && !self.word(i).is_some_and(|w| w.shown) {
                    if !self.non_word(i) {
                        curlen += 1;
                    }
                    marker = i;
                    i += 1;
                }
                i = marker;
                while i > end && self.bad_end(i) {
                    if !self.non_word(i) {
                        curlen -= 1;
                    }
                    i -= 1;
                }
                end = i;
            }
            covers[best].start = start;
            covers[best].end = end;
            covers[best].curlen = curlen;
            self.mark(all, start, end);
            count += 1;
            for (i, c) in covers.iter_mut().enumerate() {
                if i != best
                    && ((c.start >= start && c.start <= end)
                        || (c.end >= start && c.end <= end)
                        || (c.start < start && c.end > end))
                {
                    c.excluded = true;
                }
            }
        }
        if count == 0 {
            let end = self.first_words(opts.min_words);
            self.mark(all, 0, end);
        }
    }

    /// The index of the last token of the first `min_words` words, or -1 for no tokens.
    fn first_words(&self, min_words: i32) -> i32 {
        let (mut curlen, mut end, mut i) = (0, -1, 0);
        while i < self.len() && curlen < min_words {
            if !self.non_word(i) {
                curlen += 1;
            }
            end = i;
            i += 1;
        }
        end
    }

    /// `mark_hl_words`: the selector for `MaxFragments` equal to 0.
    fn mark_words(&mut self, locations: &[Phrase], opts: &Options) {
        let (min_words, max_words) = (opts.min_words, opts.max_words);
        let n = self.len();
        let (mut bestb, mut beste, mut bestlen, mut bestcover) = (-1, -1, -1, false);
        if opts.highlight_all {
            self.mark(true, 0, n - 1);
            return;
        }
        let mut next = 0;
        while let Some((p, q)) = self.cover(locations, &mut next) {
            let (mut curlen, mut poslen) = (0, 0);
            let (mut posb, mut pose) = (p, p);
            let mut i = p;
            while i <= q && curlen < max_words {
                if !self.non_word(i) {
                    curlen += 1;
                }
                if self.interesting(i) {
                    poslen += 1;
                }
                pose = i;
                i += 1;
            }
            if curlen < max_words {
                // Make the headline longer to the right up to a good end, and then to the left.
                i -= 1;
                while i < n && curlen < max_words {
                    if i > q {
                        if !self.non_word(i) {
                            curlen += 1;
                        }
                        if self.interesting(i) {
                            poslen += 1;
                        }
                    }
                    pose = i;
                    if !self.bad_end(i) && curlen >= min_words {
                        break;
                    }
                    i += 1;
                }
                if curlen < min_words {
                    i = p - 1;
                    while i >= 0 {
                        if !self.non_word(i) {
                            curlen += 1;
                        }
                        if self.interesting(i) {
                            poslen += 1;
                        }
                        if curlen >= max_words || (!self.bad_end(i) && curlen >= min_words) {
                            break;
                        }
                        i -= 1;
                    }
                    posb = i.max(0);
                }
            } else {
                // Make the headline shorter to get a good end.
                i = i.min(q);
                while curlen > min_words && self.bad_end(i) {
                    if !self.non_word(i) {
                        curlen -= 1;
                    }
                    if self.interesting(i) {
                        poslen -= 1;
                    }
                    pose = i - 1;
                    i -= 1;
                }
            }
            let poscover = posb <= p && pose >= q;
            if (poscover && !bestcover)
                || (poscover == bestcover && poslen > bestlen)
                || (poscover == bestcover
                    && poslen == bestlen
                    && !self.bad_end(pose)
                    && self.bad_end(beste))
            {
                bestb = posb;
                beste = pose;
                bestlen = poslen;
                bestcover = poscover;
            }
        }
        if bestlen < 0 {
            bestb = 0;
            beste = self.first_words(min_words);
        }
        self.mark(false, bestb, beste);
    }

    /// `generateHeadline`: the tokens in the headline, with the delimiter between fragments.
    fn generate(&self, opts: &Options) -> String {
        let mut out = String::new();
        let (mut in_fragment, mut fragments) = (false, 0);
        for word in &self.words {
            if word.shown && !word.repeated {
                if !in_fragment {
                    in_fragment = true;
                    fragments += 1;
                    if fragments > 1 {
                        out.push_str(&opts.delimiter);
                    }
                }
                if word.replace {
                    out.push(' ');
                } else if !word.skip {
                    if word.selected {
                        out.push_str(&opts.start_sel);
                    }
                    out.push_str(word.text);
                    if word.selected {
                        out.push_str(&opts.stop_sel);
                    }
                }
            } else if !word.repeated {
                in_fragment = false;
            }
        }
        out
    }
}

/// `ts_headline_byid_opt`: the headline of a text for a query with a configuration and the options.
fn headline_with(
    session: &dyn Session,
    config: u32,
    text: &str,
    items: &[QueryItem],
    opts: Option<&str>,
) -> Result<String> {
    let config = tsdict::config(session.catalog(), config)?;
    let words = parse(session, &config, items, text);
    let opts = options(opts)?;
    let mut headline = Headline { words, items, short_word: opts.short_word };
    let locations = if items.is_empty() {
        Vec::new()
    } else {
        let words = &headline.words;
        tsmatch::locations(items, 0, &|at, data| check(words, at, data)).unwrap_or_default()
    };
    if opts.max_fragments == 0 {
        headline.mark_words(&locations, &opts);
    } else {
        headline.mark_fragments(&locations, &opts);
    }
    for (name, value) in [
        ("StartSel", &opts.start_sel),
        ("StopSel", &opts.stop_sel),
        ("FragmentDelimiter", &opts.delimiter),
    ] {
        if value.len() > usize::from(i16::MAX.cast_unsigned()) {
            return Err(invalid(format!("value for \"{name}\" is too long")));
        }
    }
    Ok(headline.generate(&opts))
}

/// `ts_headline_byid_opt`, `ts_headline_byid`, `ts_headline_opt` and `ts_headline`: the forms with and without a configuration and with and without the options.
fn headline(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (config, rest) = match args {
        [_, Value::TsQuery(_), ..] => (tsearch::current_config(call.session)?, args),
        [config, rest @ ..] => (config.as_oid().ok_or_else(bad_value)?, rest),
        [] => return Err(bad_value()),
    };
    let (text, items, opts) = match rest {
        [text, Value::TsQuery(items)] => (text, items, None),
        [text, Value::TsQuery(items), opts] => {
            (text, items, Some(opts.as_str().ok_or_else(bad_value)?))
        }
        _ => return Err(bad_value()),
    };
    let text = text.as_str().ok_or_else(bad_value)?;
    Ok(Value::Text(headline_with(call.session, config, text, items, opts)?))
}

/// The kernel of a form of `ts_headline` by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "ts_headline_byid_opt" | "ts_headline_byid" | "ts_headline_opt" | "ts_headline" => headline,
        _ => return None,
    })
}
