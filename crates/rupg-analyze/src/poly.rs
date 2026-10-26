//! The polymorphic types of `parse_coerce.c`: `check_generic_type_consistency`, which tells if the actual types fit the polymorphic arguments of a function, and `enforce_generic_type_consistency`, which gives the actual argument types and the result type of a call.

use rupg_common::{Error, Result, SqlState};
use rupg_types::oid;

use crate::coerce::{Context, can_coerce, common_of};
use crate::types;

/// The facts that the actual types give about each polymorphic family.
#[derive(Default)]
struct Families {
    element: u32,
    array: u32,
    range: u32,
    multirange: u32,
    nonarray: bool,
    is_enum: bool,
    /// True when an argument or the result is `anymultirange`.
    wants_multirange: bool,
    /// The number of arguments of the first family: `anyelement`, `anyarray`, `anyrange` and the others.
    any_element_args: usize,
    compatible: Vec<u32>,
    compatible_array: bool,
    compatible_nonarray: bool,
    compatible_range: u32,
    compatible_range_element: u32,
    compatible_multirange: u32,
    /// True when an argument or the result is `anycompatiblerange`.
    wants_compatible_range: bool,
    /// True when an argument or the result is `anycompatiblemultirange`.
    wants_compatible_multirange: bool,
    has_compatible: bool,
    /// The polymorphic result type of the enforcement, or 0 for the check.
    result: u32,
}

/// The error of two arguments of one family with different types.
fn not_alike(family: &str, first: u32, second: u32) -> Error {
    Error::new(
        SqlState::DATATYPE_MISMATCH,
        format!("arguments declared \"{family}\" are not all alike"),
    )
    .with_detail(format!("{} versus {}", types::name(first), types::name(second)))
}

fn mismatch(message: String) -> Error {
    Error::new(SqlState::DATATYPE_MISMATCH, message)
}

/// The error of an argument whose type is not of the kind of its family.
fn not_kind(family: &str, kind: &str, ty: u32) -> Error {
    mismatch(format!("argument declared {family} is not {kind} but type {}", types::name(ty)))
}

/// The error of two families that give different types.
fn not_consistent(family: &str, other: &str, first: u32, second: u32) -> Error {
    mismatch(format!("argument declared {family} is not consistent with argument declared {other}"))
        .with_detail(format!("{} versus {}", types::name(first), types::name(second)))
}

/// The error of a polymorphic type that only unknown inputs give.
fn undetermined(family: &str) -> Error {
    mismatch(format!(
        "could not determine polymorphic type {family} because input has type unknown"
    ))
}

/// Keeps the type of an argument of a family, or gives the error of two different types.
fn same(slot: &mut u32, family: &str, ty: u32) -> Result<()> {
    if *slot != 0 && *slot != ty {
        return Err(not_alike(family, *slot, ty));
    }
    *slot = ty;
    Ok(())
}

/// Reads the actual types of the polymorphic arguments, and the polymorphic result type when it is not 0. An error means that the types do not fit, which the check gives as false and the enforcement gives as the error.
fn collect(inputs: &[u32], declared: &[u32], result: u32) -> Result<Families> {
    let mut f = Families {
        nonarray: result == oid::ANYNONARRAY,
        is_enum: result == oid::ANYENUM,
        wants_multirange: result == oid::ANYMULTIRANGE,
        compatible_nonarray: result == oid::ANYCOMPATIBLENONARRAY,
        compatible_array: result == oid::ANYCOMPATIBLEARRAY,
        wants_compatible_range: result == oid::ANYCOMPATIBLERANGE,
        wants_compatible_multirange: result == oid::ANYCOMPATIBLEMULTIRANGE,
        result,
        ..Families::default()
    };
    for (&input, &decl) in inputs.iter().zip(declared) {
        match decl {
            oid::ANYELEMENT | oid::ANYNONARRAY | oid::ANYENUM => {
                f.any_element_args += 1;
                f.nonarray |= decl == oid::ANYNONARRAY;
                f.is_enum |= decl == oid::ANYENUM;
                if input != oid::UNKNOWN {
                    same(&mut f.element, "anyelement", input)?;
                }
            }
            oid::ANYARRAY => {
                f.any_element_args += 1;
                if input != oid::UNKNOWN {
                    same(&mut f.array, "anyarray", types::base(input))?;
                }
            }
            oid::ANYRANGE => {
                f.any_element_args += 1;
                if input != oid::UNKNOWN {
                    same(&mut f.range, "anyrange", types::base(input))?;
                }
            }
            oid::ANYMULTIRANGE => {
                f.any_element_args += 1;
                f.wants_multirange = true;
                if input != oid::UNKNOWN {
                    same(&mut f.multirange, "anymultirange", types::base(input))?;
                }
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
                    return Err(not_kind("anycompatiblearray", "an array", types::base(input)));
                }
                f.compatible.push(element);
            }
            oid::ANYCOMPATIBLERANGE => {
                f.has_compatible = true;
                f.wants_compatible_range = true;
                if input == oid::UNKNOWN {
                    continue;
                }
                let range = types::base(input);
                if f.compatible_range != 0 {
                    same(&mut f.compatible_range, "anycompatiblerange", range)?;
                    continue;
                }
                let element = types::range_subtype(range);
                if element == 0 {
                    return Err(not_kind("anycompatiblerange", "a range type", range));
                }
                f.compatible_range = range;
                f.compatible_range_element = element;
                f.compatible.push(element);
            }
            oid::ANYCOMPATIBLEMULTIRANGE => {
                f.has_compatible = true;
                f.wants_compatible_multirange = true;
                if input == oid::UNKNOWN {
                    continue;
                }
                let multirange = types::base(input);
                if f.compatible_multirange != 0 {
                    same(&mut f.compatible_multirange, "anycompatiblemultirange", multirange)?;
                    continue;
                }
                if types::multirange_range(multirange) == 0 {
                    return Err(not_kind(
                        "anycompatiblemultirange",
                        "a multirange type",
                        multirange,
                    ));
                }
                f.compatible_multirange = multirange;
            }
            _ => {}
        }
    }
    if f.any_element_args > 0 {
        first_family(&mut f)?;
    }
    if f.has_compatible {
        compatible_ranges(&mut f)?;
    }
    Ok(f)
}

/// The checks of the first family: the element type of the array, the range of the multirange and the subtype of the range must be the element type. An `anyarray` input, such as a column of `pg_statistic`, fits an `anyarray` argument when it is the only argument of the family and the result does not need its element type. The element is then `anyelement`, as `enforce_generic_type_consistency` does, and the check allows it, as `check_generic_type_consistency` does.
fn first_family(f: &mut Families) -> Result<()> {
    if f.array == oid::ANYARRAY {
        if f.result != 0 {
            let family1 = matches!(
                f.result,
                oid::ANYELEMENT
                    | oid::ANYNONARRAY
                    | oid::ANYENUM
                    | oid::ANYRANGE
                    | oid::ANYMULTIRANGE
            );
            if f.any_element_args != 1 || family1 {
                return Err(mismatch(
                    "cannot determine element type of \"anyarray\" argument".into(),
                ));
            }
            f.element = oid::ANYELEMENT;
        }
    } else if f.array != 0 {
        let element = types::element(f.array);
        if element == 0 {
            return Err(not_kind("anyarray", "an array", f.array));
        }
        if f.element == 0 {
            f.element = element;
        } else if element != f.element {
            return Err(not_consistent("anyarray", "anyelement", f.array, f.element));
        }
    }
    if f.multirange != 0 {
        let range = types::multirange_range(f.multirange);
        if range == 0 {
            return Err(not_kind("anymultirange", "a multirange type", f.multirange));
        }
        if f.range == 0 {
            f.range = range;
        } else if range != f.range {
            return Err(not_consistent("anymultirange", "anyrange", f.multirange, f.range));
        }
    } else if f.wants_multirange && f.range != 0 {
        f.multirange = types::range_multirange(f.range);
    }
    if f.range != 0 {
        let element = types::range_subtype(f.range);
        if element == 0 {
            return Err(not_kind("anyrange", "a range type", f.range));
        }
        if f.element == 0 {
            f.element = element;
        } else if element != f.element {
            return Err(not_consistent("anyrange", "anyelement", f.range, f.element));
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
    Ok(())
}

/// The range of the `anycompatiblemultirange` argument must be the `anycompatiblerange` type, and its subtype joins the choice of the common type.
fn compatible_ranges(f: &mut Families) -> Result<()> {
    if f.compatible_multirange != 0 {
        let range = types::multirange_range(f.compatible_multirange);
        if f.compatible_range != 0 {
            if range != f.compatible_range {
                return Err(not_consistent(
                    "anycompatiblemultirange",
                    "anycompatiblerange",
                    f.compatible_multirange,
                    f.compatible_range,
                ));
            }
        } else {
            let element = types::range_subtype(range);
            if element == 0 {
                return Err(not_kind(
                    "anycompatiblemultirange",
                    "a multirange type",
                    f.compatible_multirange,
                ));
            }
            f.compatible_range = range;
            f.compatible_range_element = element;
            f.wants_compatible_range = true;
            f.compatible.push(element);
        }
    } else if f.wants_compatible_multirange && f.compatible_range != 0 {
        f.compatible_multirange = types::range_multirange(f.compatible_range);
    }
    Ok(())
}

/// The common type of the `anycompatible` family, or an error if the types have none. The subtype of an `anycompatiblerange` argument must be the common type.
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
    if f.compatible_range_element != 0 && f.compatible_range_element != common {
        return Err(mismatch(format!(
            "anycompatiblerange type {} does not match anycompatible type {}",
            types::name(f.compatible_range),
            types::name(common)
        )));
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
    let Ok(f) = collect(inputs, declared, 0) else { return false };
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
pub fn resolve(inputs: &[u32], declared: &mut [u32], result: u32) -> Result<u32> {
    let f = collect(inputs, declared, result)?;
    if f.any_element_args == 0 && !f.has_compatible {
        return Ok(result);
    }
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
        if f.wants_compatible_range && f.compatible_range == 0 {
            return Err(undetermined("anycompatiblerange"));
        }
        if f.wants_compatible_multirange && f.compatible_multirange == 0 {
            return Err(undetermined("anycompatiblemultirange"));
        }
        (common, array)
    } else {
        (0, 0)
    };
    for (decl, &input) in declared.iter_mut().zip(inputs) {
        match *decl {
            oid::ANYCOMPATIBLE | oid::ANYCOMPATIBLENONARRAY => *decl = compatible,
            oid::ANYCOMPATIBLEARRAY => *decl = compatible_array,
            oid::ANYCOMPATIBLERANGE => *decl = f.compatible_range,
            oid::ANYCOMPATIBLEMULTIRANGE => *decl = f.compatible_multirange,
            oid::ANYELEMENT | oid::ANYNONARRAY | oid::ANYENUM if input == oid::UNKNOWN => {
                *decl = element;
            }
            oid::ANYARRAY if input == oid::UNKNOWN => {
                if array == 0 {
                    array = array_of(element)?;
                }
                *decl = array;
            }
            oid::ANYRANGE if input == oid::UNKNOWN => {
                if f.range == 0 {
                    return Err(undetermined("anyrange"));
                }
                *decl = f.range;
            }
            oid::ANYMULTIRANGE if input == oid::UNKNOWN => {
                if f.multirange == 0 {
                    return Err(undetermined("anymultirange"));
                }
                *decl = f.multirange;
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
        oid::ANYRANGE if f.range == 0 => Err(undetermined("anyrange")),
        oid::ANYRANGE => Ok(f.range),
        oid::ANYMULTIRANGE if f.multirange == 0 => Err(undetermined("anymultirange")),
        oid::ANYMULTIRANGE => Ok(f.multirange),
        oid::ANYCOMPATIBLE | oid::ANYCOMPATIBLENONARRAY => Ok(compatible),
        oid::ANYCOMPATIBLEARRAY => Ok(compatible_array),
        oid::ANYCOMPATIBLERANGE => Ok(f.compatible_range),
        oid::ANYCOMPATIBLEMULTIRANGE => Ok(f.compatible_multirange),
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

    #[test]
    fn ranges() {
        assert!(consistent(&[oid::INT4RANGE, oid::INT4], &[oid::ANYRANGE, oid::ANYELEMENT]));
        assert!(!consistent(&[oid::INT4RANGE, oid::INT8], &[oid::ANYRANGE, oid::ANYELEMENT]));
        assert!(!consistent(&[oid::INT4], &[oid::ANYRANGE]));
        assert_eq!(
            resolve(&[oid::INT4RANGE], &mut [oid::ANYRANGE], oid::ANYELEMENT).unwrap(),
            oid::INT4
        );
        let mut declared = [oid::ANYRANGE, oid::ANYRANGE];
        assert_eq!(
            resolve(&[oid::DATERANGE, oid::UNKNOWN], &mut declared, oid::BOOL).unwrap(),
            oid::BOOL
        );
        assert_eq!(declared, [oid::ANYRANGE, oid::DATERANGE]);
        let err =
            resolve(&[oid::INT4RANGE, oid::INT8], &mut [oid::ANYRANGE, oid::ANYELEMENT], oid::BOOL)
                .unwrap_err();
        assert_eq!(
            err.message(),
            "argument declared anyrange is not consistent with argument declared anyelement"
        );
        let err =
            resolve(&[oid::UNKNOWN, oid::INT4], &mut [oid::ANYRANGE, oid::ANYELEMENT], oid::BOOL)
                .unwrap_err();
        assert_eq!(
            err.message(),
            "could not determine polymorphic type anyrange because input has type unknown"
        );
        let mut declared = [oid::ANYCOMPATIBLERANGE, oid::ANYCOMPATIBLE];
        assert_eq!(
            resolve(&[oid::INT4RANGE, oid::INT4], &mut declared, oid::ANYCOMPATIBLERANGE).unwrap(),
            oid::INT4RANGE
        );
        assert_eq!(declared, [oid::INT4RANGE, oid::INT4]);
        let err = resolve(
            &[oid::INT4RANGE, oid::INT8],
            &mut [oid::ANYCOMPATIBLERANGE, oid::ANYCOMPATIBLE],
            oid::BOOL,
        )
        .unwrap_err();
        assert_eq!(
            err.message(),
            "anycompatiblerange type int4range does not match anycompatible type bigint"
        );
    }
}
