//! The sort and equality operators of a type, as `lookup_type_cache` of `typcache.c` finds them, and the binary casts of `IsBinaryCoercible`.

use rupg_pgcatalog::builtin::{self, OpclassRow};
use rupg_types::oid;

use crate::types;

/// The OID of the btree access method.
pub(crate) const BTREE: u32 = 403;
/// The OID of the hash access method.
const HASH: u32 = 405;
/// The btree strategy of `<`.
pub(crate) const LESS: i16 = 1;
/// The btree strategy of `=`, which is also the hash strategy of `=`.
pub(crate) const EQUAL: i16 = 3;
/// The btree strategy of `>`.
pub(crate) const GREATER: i16 = 5;
const HASH_EQUAL: i16 = 1;
/// `ARRAY_EQ_OP`, `ARRAY_LT_OP` and `ARRAY_GT_OP`.
const ARRAY_EQ_OP: u32 = 1070;
const ARRAY_LT_OP: u32 = 1072;
const ARRAY_GT_OP: u32 = 1073;
/// `RECORD_EQ_OP`, `RECORD_LT_OP` and `RECORD_GT_OP`.
const RECORD_EQ_OP: u32 = 2988;
const RECORD_LT_OP: u32 = 2990;
const RECORD_GT_OP: u32 = 2991;

/// The `<`, `=` and `>` operators of a type, or 0 for an operator that the type does not have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Operators {
    pub(crate) lt: u32,
    pub(crate) eq: u32,
    pub(crate) gt: u32,
}

/// `IsBinaryCoercible`: true if a value of `source` is a value of `target` with no change.
pub(crate) fn is_binary_coercible(source: u32, target: u32) -> bool {
    if source == target
        || target == oid::ANY
        || target == oid::ANYELEMENT
        || target == oid::ANYCOMPATIBLE
    {
        return true;
    }
    let source = if source == 0 { source } else { types::base(source) };
    if source == target {
        return true;
    }
    let row = types::row(source);
    let kind = row.map_or(0, |t| t.kind);
    let is_array = types::element(source) != 0;
    let matched = match target {
        oid::ANYARRAY | oid::ANYCOMPATIBLEARRAY => is_array,
        oid::ANYNONARRAY | oid::ANYCOMPATIBLENONARRAY => !is_array,
        oid::ANYENUM => kind == b'e',
        oid::ANYRANGE | oid::ANYCOMPATIBLERANGE => kind == b'r',
        oid::ANYMULTIRANGE | oid::ANYCOMPATIBLEMULTIRANGE => kind == b'm',
        oid::RECORD => is_complex(source),
        oid::RECORD_ARRAY => is_complex(types::element(source)),
        _ => false,
    };
    matched || builtin::cast(source, target).is_some_and(|c| c.method == b'b' && c.context == b'i')
}

/// `ISCOMPLEX`: true for a composite type or a domain over one.
fn is_complex(ty: u32) -> bool {
    types::row(types::base(ty)).is_some_and(|t| t.relid != 0)
}

/// `GetDefaultOpClass`: the default operator class of the type for the access method. An exact match comes first. Else the one class that the type has a binary cast to, or the one for a preferred type of its category when there are more.
pub(crate) fn default_opclass(ty: u32, method: u32) -> Option<&'static OpclassRow> {
    let ty = types::base(ty);
    let category = types::category(ty);
    let (mut exact, mut compatible, mut preferred) = (0, 0, 0);
    let mut result = None;
    for class in builtin::opclasses().iter().filter(|c| c.method == method && c.default) {
        if class.intype == ty {
            exact += 1;
            result = Some(class);
        } else if exact == 0 && is_binary_coercible(ty, class.intype) {
            if types::is_preferred(category, class.intype) {
                preferred += 1;
                result = Some(class);
            } else if preferred == 0 {
                compatible += 1;
                result = Some(class);
            }
        }
    }
    (exact == 1 || preferred == 1 || (preferred == 0 && compatible == 1))
        .then_some(result)
        .flatten()
}

/// The operators of the type, as `lookup_type_cache` gives `lt_opr`, `eq_opr` and `gt_opr`. An array has the operators only when its element type has them, and the same for a record.
pub(crate) fn operators(ty: u32) -> Operators {
    let btree = default_opclass(ty, BTREE);
    let member = |strategy| {
        btree.and_then(|c| builtin::opfamily_member(c.family, c.intype, c.intype, strategy))
    };
    let mut eq = member(EQUAL).or_else(|| {
        default_opclass(ty, HASH)
            .and_then(|c| builtin::opfamily_member(c.family, c.intype, c.intype, HASH_EQUAL))
    });
    let (mut lt, mut gt) = (member(LESS), member(GREATER));
    let element = types::element(types::base(ty));
    let element_ops = || (element != 0).then(|| operators(element));
    if eq == Some(ARRAY_EQ_OP) && element_ops().is_none_or(|e| e.eq == 0) {
        eq = None;
    }
    if lt == Some(ARRAY_LT_OP) && element_ops().is_none_or(|e| e.lt == 0) {
        lt = None;
    }
    if gt == Some(ARRAY_GT_OP) && element_ops().is_none_or(|e| e.gt == 0) {
        gt = None;
    }
    // A record compares its columns, which the engine does not know for an anonymous record.
    if eq == Some(RECORD_EQ_OP) {
        eq = None;
    }
    if lt == Some(RECORD_LT_OP) {
        lt = None;
    }
    if gt == Some(RECORD_GT_OP) {
        gt = None;
    }
    Operators { lt: lt.unwrap_or(0), eq: eq.unwrap_or(0), gt: gt.unwrap_or(0) }
}

/// `get_equality_op_for_ordering_op`: the `=` operator of the btree family of an ordering operator, and true when the operator sorts in descending order. `None` when the operator is not the `<` or the `>` of a btree family.
pub(crate) fn equality_for_ordering(operator: u32) -> Option<(u32, bool)> {
    let amop = builtin::amops_of_operator(operator).find(|a| {
        a.method == BTREE && (a.strategy == LESS || a.strategy == GREATER) && a.left == a.right
    })?;
    let eq = builtin::opfamily_member(amop.family, amop.left, amop.left, EQUAL)?;
    Some((eq, amop.strategy == GREATER))
}

/// `op_hashjoinable`: true when the `=` operator can use a hash table. The `=` of arrays can only when the element type has a hash function.
pub(crate) fn hashable(eq: u32, ty: u32) -> bool {
    if eq == ARRAY_EQ_OP {
        let element = types::element(types::base(ty));
        return element != 0 && default_opclass(element, HASH).is_some();
    }
    eq != RECORD_EQ_OP && builtin::operator_by_oid(eq).is_some_and(|op| op.can_hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operators_of_types() {
        assert_eq!(operators(oid::INT4), Operators { lt: 97, eq: 96, gt: 521 });
        assert_eq!(operators(oid::TEXT), Operators { lt: 664, eq: 98, gt: 666 });
        // varchar has no class of its own and takes the class of text.
        assert_eq!(operators(oid::VARCHAR), operators(oid::TEXT));
        assert_eq!(operators(oid::INT4_ARRAY), Operators { lt: 1072, eq: 1070, gt: 1073 });
        // xid has a hash class and no btree class.
        assert_eq!(operators(oid::XID), Operators { lt: 0, eq: 352, gt: 0 });
        assert_eq!(operators(oid::POINT), Operators { lt: 0, eq: 0, gt: 0 });
        assert_eq!(equality_for_ordering(97), Some((96, false)));
        assert_eq!(equality_for_ordering(521), Some((96, true)));
        assert_eq!(equality_for_ordering(96), None);
        assert!(is_binary_coercible(oid::VARCHAR, oid::TEXT));
        assert!(is_binary_coercible(oid::INT4_ARRAY, oid::ANYARRAY));
        assert!(!is_binary_coercible(oid::INT4, oid::INT8));
        assert!(hashable(96, oid::INT4));
        assert!(hashable(1070, oid::INT4_ARRAY));
        assert!(hashable(352, oid::XID));
    }
}
