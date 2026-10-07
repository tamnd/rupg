//! SASLprep (RFC 4013), as `pg_saslprep` in `src/common/saslprep.c` does it.
//!
//! PostgreSQL applies SASLprep to a clear text password before it makes a SCRAM secret and before it checks a password against a SCRAM secret. If SASLprep fails, PostgreSQL uses the password as it is. [`prepare_password`] does the same. See `spec/06-wire-and-sessions.md` section 6.7.
//!
//! The steps follow `pg_saslprep`, also where it differs from RFC 3454: the checks for prohibited characters and for the bidirectional rules read the string after the mapping and before the normalization.

use std::borrow::Cow;

use rupg_types::unicode::{NormalForm, normalize};

use crate::generated::saslprep::{
    L_CAT, MAPPED_TO_NOTHING, NON_ASCII_SPACE, PROHIBITED_OUTPUT, RAND_AL_CAT, UNASSIGNED,
};

/// Why SASLprep did not prepare a password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaslprepError {
    /// `SASLPREP_INVALID_UTF8`: the password is not valid UTF-8.
    InvalidUtf8,
    /// `SASLPREP_PROHIBITED`: the password is empty after the mapping, has a prohibited or unassigned character, or does not obey the bidirectional rules.
    Prohibited,
}

fn in_table(c: u32, table: &[(u32, u32)]) -> bool {
    table
        .binary_search_by(|&(first, last)| {
            if last < c {
                std::cmp::Ordering::Less
            } else if first > c {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// `pg_saslprep`: the password after SASLprep.
pub fn saslprep(password: &[u8]) -> Result<String, SaslprepError> {
    let text = std::str::from_utf8(password).map_err(|_| SaslprepError::InvalidUtf8)?;

    // 1) Map.
    let mut mapped = Vec::with_capacity(text.len());
    for c in text.chars().map(u32::from) {
        if in_table(c, &NON_ASCII_SPACE) {
            mapped.push(0x20);
        } else if !in_table(c, &MAPPED_TO_NOTHING) {
            mapped.push(c);
        }
    }
    if mapped.is_empty() {
        return Err(SaslprepError::Prohibited);
    }

    // 2) Normalize.
    let output = normalize(NormalForm::Nfkc, &mapped);

    // 3) Prohibit. PostgreSQL checks the mapped string, not the output.
    if mapped.iter().any(|&c| in_table(c, &PROHIBITED_OUTPUT) || in_table(c, &UNASSIGNED)) {
        return Err(SaslprepError::Prohibited);
    }

    // 4) Check bidi, on the mapped string too.
    if mapped.iter().any(|&c| in_table(c, &RAND_AL_CAT)) {
        let ends_ok =
            in_table(mapped[0], &RAND_AL_CAT) && in_table(mapped[mapped.len() - 1], &RAND_AL_CAT);
        if !ends_ok || mapped.iter().any(|&c| in_table(c, &L_CAT)) {
            return Err(SaslprepError::Prohibited);
        }
    }

    // The tables give only scalar values, and the input was valid UTF-8, so each code point is a char.
    Ok(output.into_iter().filter_map(char::from_u32).collect())
}

/// The password that PostgreSQL gives to the SCRAM functions: the result of SASLprep, or the password as it is if SASLprep fails.
pub fn prepare_password(password: &[u8]) -> Cow<'_, [u8]> {
    match saslprep(password) {
        Ok(prepared) if prepared.as_bytes() != password => Cow::Owned(prepared.into_bytes()),
        _ => Cow::Borrowed(password),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_examples_of_rfc_4013() {
        assert_eq!(saslprep("I\u{AD}X".as_bytes()), Ok("IX".to_string()));
        assert_eq!(saslprep(b"user"), Ok("user".to_string()));
        assert_eq!(saslprep(b"USER"), Ok("USER".to_string()));
        assert_eq!(saslprep("\u{AA}".as_bytes()), Ok("a".to_string()));
        assert_eq!(saslprep("\u{2168}".as_bytes()), Ok("IX".to_string()));
        assert_eq!(saslprep(b"\x07"), Err(SaslprepError::Prohibited));
        assert_eq!(saslprep("\u{627}1".as_bytes()), Err(SaslprepError::Prohibited));
    }

    #[test]
    fn the_cases_of_postgresql() {
        assert_eq!(saslprep(b"\xff"), Err(SaslprepError::InvalidUtf8));
        assert_eq!(saslprep(b""), Err(SaslprepError::Prohibited));
        assert_eq!(saslprep("\u{AD}\u{FEFF}".as_bytes()), Err(SaslprepError::Prohibited));
        // U+200B is in C.1.2 and in B.1. PostgreSQL checks C.1.2 first, so it becomes a space.
        assert_eq!(saslprep("\u{200B}".as_bytes()), Ok(" ".to_string()));
        assert_eq!(saslprep("a\u{A0}b".as_bytes()), Ok("a b".to_string()));
        assert_eq!(saslprep("\u{627}\u{628}".as_bytes()), Ok("\u{627}\u{628}".to_string()));
        assert_eq!(saslprep("\u{627}a\u{628}".as_bytes()), Err(SaslprepError::Prohibited));
        // U+0221 is not assigned in Unicode 3.2.
        assert_eq!(saslprep("\u{221}".as_bytes()), Err(SaslprepError::Prohibited));
    }

    #[test]
    fn a_failed_password_stays_as_it_is() {
        assert_eq!(prepare_password(b"secret"), Cow::Borrowed(b"secret"));
        assert_eq!(prepare_password(b"\x07bell"), Cow::Borrowed(b"\x07bell"));
        assert_eq!(prepare_password(b"\xffbad"), Cow::Borrowed(b"\xffbad"));
        assert_eq!(&*prepare_password("\u{2168}".as_bytes()), b"IX");
        assert_eq!(&*prepare_password("e\u{301}".as_bytes()), "\u{E9}".as_bytes());
    }
}
