//! The facts of a type that the rules of `parse_coerce.c` read: the category, the preferred flag, the element type and the array type.

use rupg_pgcatalog::builtin::{self, TypeRow};
use rupg_types::oid;

/// `typcategory` of the string types.
pub const STRING: u8 = b'S';
/// `TYPCATEGORY_INVALID`, the category of an OID that is not a type.
pub const INVALID: u8 = 0;

/// `array_subscript_handler`, the `typsubscript` of a true array type.
const ARRAY_SUBSCRIPT_HANDLER: u32 = 6179;

/// The row of a type.
pub fn row(ty: u32) -> Option<&'static TypeRow> {
    builtin::type_by_oid(ty)
}

/// `get_type_category_preferred`: the category and the preferred flag.
pub fn category_preferred(ty: u32) -> (u8, bool) {
    row(ty).map_or((INVALID, false), |t| (t.category, t.preferred))
}

/// `TypeCategory`.
pub fn category(ty: u32) -> u8 {
    category_preferred(ty).0
}

/// `IsPreferredType`: true if the type is preferred in this category, or in its own category when the category is [`INVALID`].
pub fn is_preferred(category: u8, ty: u32) -> bool {
    let (own, preferred) = category_preferred(ty);
    (category == own || category == INVALID) && preferred
}

/// `getBaseType`: the base type of a domain, or the type itself.
pub fn base(ty: u32) -> u32 {
    let mut ty = ty;
    while let Some(t) = row(ty) {
        if t.kind != b'd' || t.base == 0 {
            break;
        }
        ty = t.base;
    }
    ty
}

/// `get_element_type`: the element type of a true array type, or 0. `int2vector` and `oidvector` are true arrays here.
pub fn element(ty: u32) -> u32 {
    row(ty).map_or(0, |t| {
        if t.elem != 0 && t.subscript == ARRAY_SUBSCRIPT_HANDLER { t.elem } else { 0 }
    })
}

/// `get_array_type`: the array type of a type, or 0.
pub fn array_of(ty: u32) -> u32 {
    row(ty).map_or(0, |t| t.array)
}

/// `type_is_collatable`.
pub fn collatable(ty: u32) -> bool {
    row(base(ty)).is_some_and(|t| t.collation != 0)
}

/// `IsPolymorphicType`.
pub fn is_polymorphic(ty: u32) -> bool {
    matches!(
        ty,
        oid::ANYELEMENT
            | oid::ANYARRAY
            | oid::ANYNONARRAY
            | oid::ANYENUM
            | oid::ANYRANGE
            | oid::ANYMULTIRANGE
            | oid::ANYCOMPATIBLE
            | oid::ANYCOMPATIBLEARRAY
            | oid::ANYCOMPATIBLENONARRAY
            | oid::ANYCOMPATIBLERANGE
            | oid::ANYCOMPATIBLEMULTIRANGE
    )
}

/// `format_type_be`: the name of a type in an error message.
pub fn name(ty: u32) -> String {
    rupg_types::format_type(ty).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts() {
        assert_eq!(category_preferred(oid::FLOAT8), (b'N', true));
        assert_eq!(category(oid::UNKNOWN), b'X');
        assert!(is_preferred(b'S', oid::TEXT) && !is_preferred(b'S', oid::VARCHAR));
        assert!(is_preferred(INVALID, oid::TEXT) && !is_preferred(b'N', oid::TEXT));
        assert_eq!(
            (element(oid::INT4_ARRAY), element(oid::OIDVECTOR), element(oid::NAME)),
            (23, 26, 0)
        );
        assert_eq!((array_of(oid::INT4), base(oid::INT4)), (oid::INT4_ARRAY, oid::INT4));
        assert!(collatable(oid::NAME) && !collatable(oid::INT4));
        assert_eq!(name(oid::INT4_ARRAY), "integer[]");
    }
}
