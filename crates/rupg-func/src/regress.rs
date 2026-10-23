//! The functions of `regress.c`, the library of the regression tests of PostgreSQL. A function in C that links to one of them in that library runs the kernel here.

use rupg_common::Result;
use rupg_types::Value;

use crate::describe::oid_arg;
use crate::geo::{Lseg, lseg_interpt_lseg, path_points, point_value};
use crate::{Call, Kernel, bad_value};

/// `IsCatalogTextUniqueIndexOid`: the unique indexes of the catalog on a column of `text`, `pg_parameter_acl_parname_index`, `pg_replication_origin_roname_index`, `pg_seclabel_object_index` and `pg_shseclabel_object_index`.
const TEXT_UNIQUE_INDEXES: [u32; 4] = [6246, 6002, 3597, 3593];

/// The kernel of a function of `regress.c` by its link symbol.
pub(crate) fn by_symbol(symbol: &str) -> Option<Kernel> {
    let kernel: Kernel = match symbol {
        "interpt_pp" => interpt_pp,
        "reverse_name" => reverse_name,
        "test_canonicalize_path" => |_, a| Ok(Value::text(canonicalize_path(text(a)?))),
        "test_atomic_ops" => |_, _| Ok(Value::Bool(true)),
        "test_fdw_handler" => {
            |_, _| Err(rupg_common::Error::internal("test_fdw_handler is not implemented"))
        }
        "is_catalog_text_unique_index_oid" => {
            |_, a| Ok(Value::Bool(TEXT_UNIQUE_INDEXES.contains(&oid_arg(a, 0)?)))
        }
        "test_opclass_options_func" => |_, _| Ok(Value::Null),
        "test_bytea_to_text" => test_bytea_to_text,
        "test_text_to_bytea" => |_, a| Ok(Value::Bytea(text(a)?.as_bytes().to_vec())),
        "test_valid_server_encoding" => {
            |_, a| Ok(Value::Bool(crate::encoding::valid_server_encoding(text(a)?)))
        }
        "binary_coercible" => |_, a| {
            Ok(Value::Bool(rupg_analyze::is_binary_coercible(oid_arg(a, 0)?, oid_arg(a, 1)?)))
        },
        _ => return None,
    };
    Some(kernel)
}

/// The text of the first argument.
fn text(args: &[Value]) -> Result<&str> {
    match args.first() {
        Some(Value::Text(text)) => Ok(text),
        _ => Err(bad_value()),
    }
}

/// `interpt_pp`: the point where the first segment of the first path that crosses a segment of the second path crosses it, or null.
fn interpt_pp(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (p1, p2) = (path_points(args, 0)?, path_points(args, 1)?);
    for s in p1.windows(2) {
        for t in p2.windows(2) {
            let found = lseg_interpt_lseg(Lseg { p: [s[0], s[1]] }, Lseg { p: [t[0], t[1]] })?;
            if let Some(p) = found {
                return Ok(point_value(p));
            }
        }
    }
    Ok(Value::Null)
}

/// `reverse_name`: the characters of the name in the reverse order.
fn reverse_name(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::text(text(args)?.chars().rev().collect::<String>()))
}

/// `test_bytea_to_text`: the bytes as a text. PostgreSQL does not check the bytes, but a text of rupg is UTF-8, so bytes that are not UTF-8 give the error of a bad character.
fn test_bytea_to_text(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(Value::Bytea(bytes)) = args.first() else { return Err(bad_value()) };
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(Value::text(text)),
        Err(error) => Err(crate::encoding::invalid("UTF8", &bytes[error.valid_up_to()..])),
    }
}

/// `canonicalize_path` of `path.c`: the path with no `.`, no empty name and no trailing slash, and with each `..` that follows a name removed with the name. A `..` at the start of an absolute path goes away, and a `..` at the start of a relative path stays. A relative path that becomes empty is `.`, and an empty path stays empty.
fn canonicalize_path(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let absolute = path.starts_with('/');
    let (mut ups, mut names) = (0, Vec::new());
    for name in path.split('/').filter(|name| !name.is_empty() && *name != ".") {
        if name != ".." {
            names.push(name);
        } else if names.pop().is_none() && !absolute {
            ups += 1;
        }
    }
    let parts: Vec<&str> = std::iter::repeat_n("..", ups).chain(names).collect();
    match (absolute, parts.is_empty()) {
        (true, _) => format!("/{}", parts.join("/")),
        (false, true) => ".".to_string(),
        (false, false) => parts.join("/"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each function that has a kernel is a function of the library.
    #[test]
    fn symbols() {
        for symbol in rupg_analyze::REGRESS_SYMBOLS {
            let _ = by_symbol(symbol);
        }
        let kernels = [
            "interpt_pp",
            "reverse_name",
            "test_canonicalize_path",
            "test_atomic_ops",
            "test_fdw_handler",
            "is_catalog_text_unique_index_oid",
            "test_opclass_options_func",
            "test_bytea_to_text",
            "test_text_to_bytea",
            "test_valid_server_encoding",
            "binary_coercible",
        ];
        for symbol in kernels {
            assert!(by_symbol(symbol).is_some(), "{symbol}");
            assert!(rupg_analyze::REGRESS_SYMBOLS.contains(&symbol), "{symbol}");
        }
    }

    /// The cases of `misc_functions.sql`.
    #[test]
    fn canonical_paths() {
        let cases = [
            ("/", "/"),
            ("/./abc/def/", "/abc/def"),
            ("/./../abc/def", "/abc/def"),
            ("/./../../abc/def/", "/abc/def"),
            ("/abc/.././def/ghi", "/def/ghi"),
            ("/abc/./../def/ghi//", "/def/ghi"),
            ("/abc/def/../..", "/"),
            ("/abc/def/../../..", "/"),
            ("/abc/def/../../../../ghi/jkl", "/ghi/jkl"),
            (".", "."),
            ("./", "."),
            ("./abc/..", "."),
            ("abc/../", "."),
            ("abc/../def", "def"),
            ("..", ".."),
            ("../abc/def", "../abc/def"),
            ("../abc/..", ".."),
            ("../abc/../def", "../def"),
            ("../abc/../../def/ghi", "../../def/ghi"),
            ("./abc/./def/.", "abc/def"),
            ("./abc/././def/.", "abc/def"),
            ("./abc/./def/.././ghi/../../../jkl/mno", "../jkl/mno"),
        ];
        for (path, want) in cases {
            assert_eq!(canonicalize_path(path), want, "{path}");
        }
    }
}
