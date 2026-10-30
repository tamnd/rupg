//! A query identifier from the tokens of a statement, for `compute_query_id`.

use crate::{Lexer, Value, character, token};

/// The start value of FNV-1a with 64 bits.
const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// The prime of FNV-1a with 64 bits.
const PRIME: u64 = 0x0100_0000_01b3;

/// The query identifier of a statement, as `compute_query_id` gives it. PostgreSQL hashes the tree of the query after the analysis. This hashes the tokens of the text. Each constant, also `TRUE`, `FALSE` and `NULL`, counts as the same token, as `JumbleQuery` records only the place of a constant. An identifier counts in lower case, and white space and comments do not count. So two statements that differ only in their constants, in the case of their words or in their spaces get the same identifier, as in `pg_stat_statements`. `EXPLAIN` and its options do not count. The result is never 0, because 0 means no identifier.
pub fn query_id(text: &str) -> i64 {
    let open = character(b'(');
    let close = character(b')');
    let mut lexer = Lexer::new(text);
    let mut tokens =
        std::iter::from_fn(|| lexer.next_token().ok().filter(|t| t.kind != 0)).peekable();
    // The options of `EXPLAIN` in parentheses, or the words `ANALYZE` and `VERBOSE`.
    if tokens.next_if(|t| t.kind == token::EXPLAIN).is_some() {
        if tokens.next_if(|t| Some(t.kind) == open).is_some() {
            let mut depth = 1;
            while depth > 0 {
                let Some(t) = tokens.next() else { break };
                if Some(t.kind) == open {
                    depth += 1;
                } else if Some(t.kind) == close {
                    depth -= 1;
                }
            }
        }
        while tokens
            .next_if(|t| matches!(t.kind, token::ANALYZE | token::ANALYSE | token::VERBOSE))
            .is_some()
        {}
    }
    let mut hash = OFFSET;
    let mut add = |bytes: &[u8]| {
        for &b in bytes {
            hash = (hash ^ u64::from(b)).wrapping_mul(PRIME);
        }
    };
    for t in tokens {
        match t.kind {
            token::ICONST
            | token::FCONST
            | token::SCONST
            | token::USCONST
            | token::BCONST
            | token::XCONST
            | token::TRUE_P
            | token::FALSE_P
            | token::NULL_P => add(b"\x00const"),
            _ => {
                add(&t.kind.to_le_bytes());
                match &t.value {
                    Value::Integer(n) => add(&n.to_le_bytes()),
                    Value::Text(s) => add(s.as_bytes()),
                    Value::None | Value::Keyword(_) => {}
                }
            }
        }
    }
    match hash as i64 {
        0 => 1,
        id => id,
    }
}

#[cfg(test)]
mod tests {
    use super::query_id;

    #[test]
    fn same_shape_same_id() {
        let a = query_id("select * from t where a = 1 and b = 'x'");
        assert_eq!(a, query_id("SELECT *  FROM T\n WHERE a = 42 AND b = 'yz' -- note"));
        assert_eq!(
            a,
            query_id("explain (verbose, costs off) select * from t where a = 7 and b = 'q'")
        );
        assert_eq!(a, query_id("explain analyze verbose select * from t where a = 7 and b = 'q'"));
        assert_ne!(a, query_id("select * from u where a = 1 and b = 'x'"));
        assert_ne!(query_id("select $1"), query_id("select $2"));
        assert_ne!(query_id("select 1"), 0);
    }
}
