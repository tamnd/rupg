//! The polymorphic types of `parse_coerce.c`: `check_generic_type_consistency`, which tells if the actual types fit the polymorphic arguments of a function, and `enforce_generic_type_consistency`, which gives the actual argument types and the result type of a call.
//!
//! The range and multirange families need `pg_range`, which comes with the range types. Until then an argument of a range family does not fit.

use rupg_common::{Error, Result, SqlState};
use rupg_types::oid;

use crate::coerce::{Context, can_coerce, common_of};
use crate::types;

/// The facts that the actual types give about each polymorphic family.
#[derive(Default)]
struct Families {
    element: u32,
    array: u32,
    nonarray: bool,
    is_enum: bool,
    any_element_args: usize,
    compatible: Vec<u32>,
    compatible_array: bool,
    compatible_nonarray: bool,
    has_compatible: bool,
    unknowns: bool,
}

/// The error of two arguments of one family with different types.
fn not_alike(family: &str) -> Error {
    Error::new(
        SqlState::DATATYPE_MISMATCH,
        format!("arguments declared \"{family}\" are not all alike"),
    )
}

fn mismatch(message: String) -> Error {
    Error::new(SqlState::DATATYPE_MISMATCH, message)
}

/// Reads the actual types of the polymorphic arguments. `Ok(None)` means that the types do not fit, which the check gives as false and the enforcement gives as an error.
fn collect(inputs: &[u32], declared: &[u32]) -> Result<Families> {
    let mut f = Families::default();
    for (&input, &decl) in inputs.iter().zip(declared) {
        match decl {
            oid::ANYELEMENT | oid::ANYNONARRAY | oid::ANYENUM => {
                f.any_element_args += 1;
                f.nonarray |= decl == oid::ANYNONARRAY;
                f.is_enum |= decl == oid::ANYENUM;
                if input == oid::UNKNOWN {
                    f.unknowns = true;
                    continue;
                }
                if f.element != 0 && input != f.element {
                    let detail =
                        format!("{} versus {}", types::name(f.element), types::name(input));
                    return Err(not_alike("anyelement").with_detail(detail));
                }
                f.element = input;
            }
            oid::ANYARRAY => {
                f.any_element_args += 1;
                if input == oid::UNKNOWN {
                    f.unknowns = true;
                    continue;
                }
                if f.array != 0 && input != f.array {
                    let detail = format!("{} versus {}", types::name(f.array), types::name(input));
                    return Err(not_alike("anyarray").with_detail(detail));
                }
                f.array = input;
            }
            oid::ANYRANGE
            | oid::ANYMULTIRANGE
            | oid::ANYCOMPATIBLERANGE
            | oid::ANYCOMPATIBLEMULTIRANGE => {
                f.any_element_args += 1;
                if input == oid::UNKNOWN {
                    f.unknowns = true;
                    continue;
                }
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("range types are not supported yet: {}", types::name(input)),
                ));
            }
            oid::ANYCOMPATIBLE | oid::ANYCOMPATIBLENONARRAY => {
                f.has_compatible = true;
                f.compatible_nonarray |= decl == oid::ANYCOMPATIBLENONARRAY;
                if input != oid::UNKNOWN {
                    f.compatible.push(input);
                }
            }
            oid::ANYCOMPATIBLEARRAY => {
                f.has_compatible = true;
                f.compatible_array = true;
                if input == oid::UNKNOWN {
                    continue;
                }
                let element = types::element(types::base(input));
                if element == 0 {
                    return Err(mismatch(format!(
                        "argument declared anycompatiblearray is not an array but type {}",
                        types::name(input)
                    )));
                }
                f.compatible.push(element);
            }
            _ => {}
        }
    }
    if f.array != 0 {
        let element = types::element(types::base(f.array));
        if element == 0 {
            return Err(mismatch(format!(
                "argument declared anyarray is not an array but type {}",
                types::name(f.array)
            )));
        }
        if f.element == 0 {
            f.element = element;
        } else if element != f.element {
            return Err(mismatch(
                "argument declared anyarray is not consistent with argument declared anyelement"
                    .into(),
            )
            .with_detail(format!(
                "{} versus {}",
                types::name(f.array),
                types::name(f.element)
            )));
        }
    }
    if f.nonarray && f.element != 0 && types::element(types::base(f.element)) != 0 {
        return Err(mismatch(format!(
            "type matched to anynonarray is an array type: {}",
            types::name(f.element)
        )));
    }
    if f.is_enum && f.element != 0 && types::row(f.element).is_none_or(|t| t.kind != b'e') {
        return Err(mismatch(format!(
            "type matched to anyenum is not an enum type: {}",
            types::name(f.element)
        )));
    }
    Ok(f)
}

/// The common type of the `anycompatible` family, or an error if the types have none.
fn compatible_type(f: &Families) -> Result<u32> {
    if f.compatible.is_empty() {
        return Ok(oid::TEXT);
    }
    let common = common_of(&f.compatible).map_err(|(p, n, _)| {
        mismatch(format!(
            "argument types {} and {} cannot be matched",
            types::name(p),
            types::name(n)
        ))
    })?;
    if !f.compatible.iter().all(|&t| can_coerce(&[t], &[common], Context::Implicit)) {
        return Err(mismatch(
            "arguments of anycompatible family cannot be cast to a common type".into(),
        ));
    }
    if f.compatible_nonarray && types::element(types::base(common)) != 0 {
        return Err(mismatch(format!(
            "type matched to anycompatiblenonarray is an array type: {}",
            types::name(common)
        )));
    }
    Ok(common)
}

/// `check_generic_type_consistency`: true if the actual types fit the polymorphic arguments.
pub(crate) fn consistent(inputs: &[u32], declared: &[u32]) -> bool {
    let Ok(f) = collect(inputs, declared) else { return false };
    !f.has_compatible || f.compatible.is_empty() || compatible_type(&f).is_ok()
}

/// `get_array_type` with the error of PostgreSQL when the type has no array type.
fn array_of(element: u32) -> Result<u32> {
    match types::array_of(element) {
        0 => Err(Error::new(
            SqlState::UNDEFINED_OBJECT,
            format!("could not find array type for data type {}", types::name(element)),
        )),
        array => Ok(array),
    }
}

/// `enforce_generic_type_consistency`: replaces the polymorphic types in `declared` with the actual types, and gives the actual result type for `result`.
pub(crate) fn resolve(inputs: &[u32], declared: &mut [u32], result: u32) -> Result<u32> {
    let f = collect(inputs, declared)?;
    let element = f.element;
    let mut array = f.array;
    if f.any_element_args > 0 && element == 0 {
        return Err(mismatch(
            "could not determine polymorphic type because input has type unknown".into(),
        ));
    }
    let (compatible, compatible_array) = if f.has_compatible {
        let common = compatible_type(&f)?;
        let array = if f.compatible_array { array_of(common)? } else { 0 };
        (common, array)
    } else {
        (0, 0)
    };
    for (decl, &input) in declared.iter_mut().zip(inputs) {
        match *decl {
            oid::ANYCOMPATIBLE | oid::ANYCOMPATIBLENONARRAY => *decl = compatible,
            oid::ANYCOMPATIBLEARRAY => *decl = compatible_array,
            oid::ANYELEMENT | oid::ANYNONARRAY | oid::ANYENUM if input == oid::UNKNOWN => {
                *decl = element
            }
            oid::ANYARRAY if input == oid::UNKNOWN => {
                if array == 0 {
                    array = array_of(element)?;
                }
                *decl = array;
            }
            _ => {}
        }
    }
    match result {
        oid::ANYELEMENT | oid::ANYNONARRAY | oid::ANYENUM => Ok(element),
        oid::ANYARRAY => {
            if array == 0 {
                array = array_of(element)?;
            }
            Ok(array)
        }
        oid::ANYCOMPATIBLE | oid::ANYCOMPATIBLENONARRAY => Ok(compatible),
        oid::ANYCOMPATIBLEARRAY => Ok(compatible_array),
        oid::ANYRANGE
        | oid::ANYMULTIRANGE
        | oid::ANYCOMPATIBLERANGE
        | oid::ANYCOMPATIBLEMULTIRANGE => {
            Err(Error::new(SqlState::FEATURE_NOT_SUPPORTED, "range types are not supported yet"))
        }
        _ => Ok(result),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families() {
        assert!(consistent(&[oid::INT4_ARRAY, oid::INT4], &[oid::ANYARRAY, oid::ANYELEMENT]));
        assert!(!consistent(&[oid::INT4_ARRAY, oid::TEXT], &[oid::ANYARRAY, oid::ANYELEMENT]));
        assert!(!consistent(&[oid::INT4, oid::TEXT], &[oid::ANYELEMENT, oid::ANYELEMENT]));
        assert!(consistent(&[oid::INT4, oid::INT8], &[oid::ANYCOMPATIBLE, oid::ANYCOMPATIBLE]));
        let mut declared = [oid::ANYCOMPATIBLEARRAY, oid::ANYCOMPATIBLE];
        assert_eq!(
            resolve(&[oid::INT4_ARRAY, oid::INT8], &mut declared, oid::ANYCOMPATIBLEARRAY).unwrap(),
            oid::INT8_ARRAY
        );
        assert_eq!(declared, [oid::INT8_ARRAY, oid::INT8]);
        let mut declared = [oid::ANYARRAY, oid::ANYELEMENT];
        assert_eq!(
            resolve(&[oid::UNKNOWN, oid::INT4], &mut declared, oid::ANYARRAY).unwrap(),
            oid::INT4_ARRAY
        );
        assert_eq!(declared, [oid::INT4_ARRAY, oid::ANYELEMENT]);
        let err = resolve(&[oid::UNKNOWN], &mut [oid::ANYELEMENT], oid::ANYELEMENT).unwrap_err();
        assert_eq!(
            err.message(),
            "could not determine polymorphic type because input has type unknown"
        );
    }
}
