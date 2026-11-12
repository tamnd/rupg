//! The kernels that several extensions share.

use rupg_common::{Error, Result, SqlState};
use rupg_func::Call;
use rupg_types::Value;

/// The kernel of a support function of an index access method, such as the `compress` function of a GiST operator class. Such a function takes an argument of the type `internal`, which no SQL expression can give, so only an access method can call it. The executor of rupg reads an index of an extension's operator class through the operators of the class, so it never calls these.
pub(crate) fn index_support(_: &Call<'_>, _: &[Value]) -> Result<Value> {
    Err(Error::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        "an index support function can only be called by an index access method",
    ))
}

/// The C symbols of the functions of a script, as `CREATE FUNCTION ... AS 'MODULE_PATHNAME'` or `AS 'MODULE_PATHNAME', 'symbol'` names them. A function with one item of `AS` has the symbol of its name.
#[cfg(test)]
pub(crate) fn c_symbols(text: &str) -> Vec<String> {
    let mut symbols = Vec::new();
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lower[from..].find("create function") {
        let start = from + at;
        let end = lower[start..].find(';').map_or(lower.len(), |end| start + end);
        let statement = &text[start..end];
        from = end;
        let Some(module) = statement.find("'MODULE_PATHNAME'") else { continue };
        let after = statement[module + "'MODULE_PATHNAME'".len()..].trim_start();
        if let Some(rest) = after.strip_prefix(',') {
            let rest = rest.trim_start().trim_start_matches('\'');
            let symbol: String = rest.chars().take_while(|c| *c != '\'').collect();
            symbols.push(symbol);
        } else {
            let head = statement["create function".len()..].trim_start();
            let name: String = head
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.')
                .collect();
            let name = name.rsplit('.').next().unwrap_or_default().to_ascii_lowercase();
            symbols.push(name);
        }
    }
    symbols
}
