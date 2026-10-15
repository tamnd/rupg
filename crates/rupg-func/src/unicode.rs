//! The Unicode functions of `varlena.c`: `normalize`, `is_normalized`, `unicode_assigned`, `unicode_version` and `icu_unicode_version`. The database encoding of rupg is always UTF-8, so the errors of PostgreSQL for another encoding do not occur.

use rupg_common::{Error, Result, SqlState};
use rupg_types::Value;
use rupg_types::unicode::{self, NormalForm};

use crate::{Call, Kernel, bad_value};

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "unicode_normalize_func" => normalize,
        "unicode_is_normalized" => is_normalized,
        "unicode_assigned" => assigned,
        "unicode_version" => |_, _| Ok(Value::text(unicode::VERSION)),
        // rupg has no ICU, so the version of Unicode of ICU is null, as in a PostgreSQL build without ICU.
        "icu_unicode_version" => |_, _| Ok(Value::Null),
        _ => return None,
    })
}

/// `unicode_norm_form_from_string`: the form of the second argument, in any case.
fn form(args: &[Value]) -> Result<NormalForm> {
    let text = args.get(1).and_then(Value::as_str).ok_or_else(bad_value)?;
    Ok(match text.to_ascii_uppercase().as_str() {
        "NFC" => NormalForm::Nfc,
        "NFD" => NormalForm::Nfd,
        "NFKC" => NormalForm::Nfkc,
        "NFKD" => NormalForm::Nfkd,
        _ => {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("invalid normalization form: {text}"),
            ));
        }
    })
}

/// `normalize(text, form)`.
fn normalize(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let form = form(args)?;
    let text = args.first().and_then(Value::as_str).ok_or_else(bad_value)?;
    Ok(Value::Text(unicode::normalize_str(form, text)))
}

/// `is_normalized(text, form)` and `text IS NORMALIZED`: true when the normal form of the text is the text. PostgreSQL first tries the quick check of UAX 15, which gives the same answer.
fn is_normalized(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let form = form(args)?;
    let text = args.first().and_then(Value::as_str).ok_or_else(bad_value)?;
    Ok(Value::Bool(unicode::normalize_str(form, text) == text))
}

/// `unicode_assigned(text)`: true when each code point of the text is assigned.
fn assigned(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let text = args.first().and_then(Value::as_str).ok_or_else(bad_value)?;
    Ok(Value::Bool(text.chars().all(|c| unicode::is_assigned(u32::from(c)))))
}
