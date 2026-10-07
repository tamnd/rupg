//! The command tags and the text of `CommandComplete`, as `src/backend/tcop/cmdtag.c` makes it.
//!
//! The tags come from `cmdtaglist.h` at the pin, so a tag that PostgreSQL has and rupg does not is a build error and not a wrong answer at run time.
//!
//! Lifted from `crates/rudb-pgwire/src/cmdtag.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use crate::backend::OutBuf;
pub use crate::generated::cmdtag::CommandTag;
use crate::generated::cmdtag::TAGS;

impl CommandTag {
    /// The text of the tag, for example `CREATE TABLE`.
    pub fn name(self) -> &'static str {
        TAGS[self as usize].1
    }

    /// True when an event trigger can fire for the tag.
    pub fn event_trigger_ok(self) -> bool {
        TAGS[self as usize].2
    }

    /// True when a `table_rewrite` event trigger can fire for the tag.
    pub fn table_rewrite_ok(self) -> bool {
        TAGS[self as usize].3
    }

    /// True when `CommandComplete` shows the number of rows after the name, as for `SELECT`, `INSERT`, `UPDATE`, `DELETE`, `MERGE`, `COPY`, `FETCH` and `MOVE`.
    pub fn rowcount(self) -> bool {
        TAGS[self as usize].4
    }

    /// `GetCommandTagEnum`: the tag with this name, with no regard to case, or [`CommandTag::Unknown`].
    pub fn from_name(name: &str) -> CommandTag {
        let name = name.as_bytes();
        let found = TAGS.binary_search_by(|(_, tag, ..)| {
            let tag = tag.bytes().map(|b| b.to_ascii_lowercase());
            tag.cmp(name.iter().map(|b| b.to_ascii_lowercase()))
        });
        match found {
            Ok(i) => TAGS[i].0,
            Err(_) => CommandTag::Unknown,
        }
    }

    /// `BuildQueryCompletionString`: the name, and for a tag with a row count, a space and the count. `INSERT` has a `0` before the count, where PostgreSQL 11 and older put the OID of the new row.
    pub fn completion(self, rows: u64, out: &mut Vec<u8>) {
        out.extend_from_slice(self.name().as_bytes());
        if self.rowcount() {
            if self == CommandTag::Insert {
                out.extend_from_slice(b" 0");
            }
            out.push(b' ');
            let mut digits = [0u8; 20];
            let mut at = digits.len();
            let mut rest = rows;
            loop {
                at -= 1;
                digits[at] = b'0' + (rest % 10) as u8;
                rest /= 10;
                if rest == 0 {
                    break;
                }
            }
            out.extend_from_slice(&digits[at..]);
        }
    }
}

impl OutBuf {
    /// `CommandComplete` for a statement with `tag` that processed `rows` rows. The count shows only for the tags that have one, as [`CommandTag::completion`] says.
    pub fn command_tag(&mut self, tag: CommandTag, rows: u64) {
        let mark = self.begin(b'C');
        let mut text = Vec::with_capacity(32);
        tag.completion(rows, &mut text);
        self.put_string(&text);
        self.finish(mark);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;

    fn text(tag: CommandTag, rows: u64) -> String {
        let mut out = Vec::new();
        tag.completion(rows, &mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn the_table_is_in_the_order_of_the_enum() {
        for (i, (tag, ..)) in TAGS.iter().enumerate() {
            assert_eq!(*tag as usize, i);
        }
        assert_eq!(TAGS.len(), 195);
    }

    #[test]
    fn completion_strings() {
        assert_eq!(text(CommandTag::Insert, 5), "INSERT 0 5");
        assert_eq!(text(CommandTag::Select, 0), "SELECT 0");
        assert_eq!(text(CommandTag::Update, u64::MAX), "UPDATE 18446744073709551615");
        assert_eq!(text(CommandTag::Merge, 3), "MERGE 3");
        assert_eq!(text(CommandTag::Copy, 1), "COPY 1");
        assert_eq!(text(CommandTag::Fetch, 2), "FETCH 2");
        assert_eq!(text(CommandTag::Move, 2), "MOVE 2");
        // PostgreSQL ends `CREATE TABLE AS` and `SELECT INTO` with the tag `SELECT` and the count. These two tags have no count, for the event triggers that see them.
        assert_eq!(text(CommandTag::CreateTableAs, 9), "CREATE TABLE AS");
        assert_eq!(text(CommandTag::Begin, 1), "BEGIN");
        assert_eq!(text(CommandTag::StartTransaction, 1), "START TRANSACTION");
        assert_eq!(text(CommandTag::Unknown, 1), "???");
    }

    #[test]
    fn names_are_found_with_no_regard_to_case() {
        assert_eq!(CommandTag::from_name("CREATE TABLE"), CommandTag::CreateTable);
        assert_eq!(CommandTag::from_name("create table"), CommandTag::CreateTable);
        assert_eq!(CommandTag::from_name("???"), CommandTag::Unknown);
        assert_eq!(CommandTag::from_name(""), CommandTag::Unknown);
        assert_eq!(CommandTag::from_name("CREATE TABLES"), CommandTag::Unknown);
        for (tag, name, ..) in TAGS {
            assert_eq!(CommandTag::from_name(name), tag);
        }
    }

    #[test]
    fn command_complete_has_the_tag() {
        let mut out = OutBuf::new();
        out.command_tag(CommandTag::Delete, 7);
        let (message, _) = Backend::decode(out.as_bytes()).unwrap().unwrap();
        assert_eq!(message, Backend::CommandComplete(b"DELETE 7"));
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn the_table_is_the_vendored_file() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../vendor/postgres-19/src/include/tcop/cmdtaglist.h"
        );
        let text = std::fs::read_to_string(path).unwrap();
        let rows: Vec<String> = text
            .lines()
            .filter_map(|l| l.strip_prefix("PG_CMDTAG("))
            .map(|l| {
                let (_, rest) = l.split_once(", ").unwrap();
                rest.trim_end_matches(')').to_owned()
            })
            .collect();
        let ours: Vec<String> =
            TAGS.iter().map(|(_, name, a, b, c)| format!("\"{name}\", {a}, {b}, {c}")).collect();
        assert_eq!(rows, ours);
    }
}
