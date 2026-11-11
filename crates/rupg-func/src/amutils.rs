//! The properties of the index access methods, of the indexes and of the index columns: `pg_indexam_has_property`, `pg_index_has_property` and `pg_index_column_has_property`. A port of `amutils.c` with the fields of the `IndexAmRoutine` of each built-in access method and its `amproperty` function.

use rupg_catalog::RelKind;
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_types::Value;

use crate::{BrinRanges, Call, Kernel, Session, bad_value};

/// `INDOPTION_DESC`.
const DESC: i16 = 1;
/// `INDOPTION_NULLS_FIRST`.
const NULLS_FIRST: i16 = 2;
/// The OID of `btree`.
const BTREE: u32 = 403;
/// The OID of `gist`.
const GIST: u32 = 783;
/// The OID of `spgist`.
const SPGIST: u32 = 4000;
/// `GIST_COMPRESS_PROC`.
const GIST_COMPRESS: i16 = 3;
/// `GIST_DISTANCE_PROC`.
const GIST_DISTANCE: i16 = 8;
/// `GIST_FETCH_PROC`.
const GIST_FETCH: i16 = 9;

/// The fields of an `IndexAmRoutine` that the properties read.
struct Routine {
    can_order: bool,
    can_order_by_op: bool,
    can_backward: bool,
    can_unique: bool,
    can_multi_col: bool,
    search_array: bool,
    search_nulls: bool,
    clusterable: bool,
    can_include: bool,
    /// `amgettuple` is set.
    get_tuple: bool,
    /// `amcanreturn` is set.
    can_return: bool,
}

/// `GetIndexAmRoutineByAmId` with `noerror`: the routine of a built-in index access method, or `None` for an OID that is not an index access method.
fn routine(am: u32) -> Option<Routine> {
    let r = |flags: [bool; 11]| Routine {
        can_order: flags[0],
        can_order_by_op: flags[1],
        can_backward: flags[2],
        can_unique: flags[3],
        can_multi_col: flags[4],
        search_array: flags[5],
        search_nulls: flags[6],
        clusterable: flags[7],
        can_include: flags[8],
        get_tuple: flags[9],
        can_return: flags[10],
    };
    let (t, f) = (true, false);
    Some(match am {
        BTREE => r([t, f, t, t, t, t, t, t, t, t, t]),
        // hash
        405 => r([f, f, t, f, f, f, f, f, f, t, f]),
        GIST => r([f, t, f, f, t, f, t, t, t, t, t]),
        // gin
        2742 => r([f, f, f, f, t, f, f, f, f, f, f]),
        SPGIST => r([f, t, f, f, f, f, t, f, t, t, t]),
        // brin
        3580 => r([f, f, f, f, t, f, t, f, f, f, f]),
        _ => return None,
    })
}

/// The facts of an index that the properties read: the access method, the number of columns, the number of key columns, and `indclass` and `indoption` of the key columns.
struct Index {
    am: u32,
    natts: i16,
    keys: i16,
    classes: Vec<u32>,
    options: Vec<i16>,
}

/// The index with this OID, from the catalog of the session or from the built-in catalogs. A built-in index is a `btree` index with no `INCLUDE` columns and the default options. `None` when the OID is not an index.
fn index(oid: u32, session: &dyn Session) -> Option<Index> {
    if let Some(rel) = session.catalog().and_then(|c| c.relation(oid)) {
        if !matches!(rel.kind, RelKind::Index | RelKind::PartitionedIndex) {
            return None;
        }
        let info = rel.index.as_ref()?;
        return Some(Index {
            am: info.method,
            natts: i16::try_from(info.keys.len()).unwrap_or(i16::MAX),
            keys: info.key_count,
            classes: info.classes.clone(),
            options: info.options.clone(),
        });
    }
    let row = builtin::class_by_oid(oid).filter(|row| row.kind == b'i')?;
    Some(Index {
        am: BTREE,
        natts: row.natts,
        keys: row.natts,
        classes: Vec::new(),
        options: vec![0; usize::try_from(row.natts).unwrap_or(0)],
    })
}

/// `amproperty` of `btree`, `gist` and `spgist` for a column: `Some` with the answer, which can be null, or `None` for the generic answer.
fn am_property(index: &Index, attno: i16, prop: &str) -> Option<Option<bool>> {
    match (index.am, prop) {
        // `btproperty`: a btree index can always give the data of a column.
        (BTREE, "returnable") => Some(Some(true)),
        // `gistproperty`: the support function of the operator class of the column.
        (GIST, "returnable" | "distance_orderable") => {
            let class = opclass(index, attno)?;
            let has = |number: i16| {
                builtin::amprocs(class.family, class.intype)
                    .iter()
                    .any(|p| p.right == class.intype && p.number == number)
            };
            Some(Some(if prop == "returnable" {
                has(GIST_FETCH) || !has(GIST_COMPRESS)
            } else {
                has(GIST_DISTANCE)
            }))
        }
        // `spgproperty`: an ordering operator of the family for the type of the column.
        (SPGIST, "distance_orderable") => {
            let class = opclass(index, attno)?;
            Some(Some(builtin::amops_of_family(class.family).any(|op| {
                op.purpose == b'o' && (op.left == class.intype || op.right == class.intype)
            })))
        }
        _ => None,
    }
}

/// `get_index_column_opclass` for a key column. An `INCLUDE` column has no operator class, and the answer of `amproperty` is then null.
fn opclass(index: &Index, attno: i16) -> Option<&'static builtin::OpclassRow> {
    let at = usize::try_from(attno - 1).ok()?;
    let oid = *index.classes.get(at)?;
    builtin::opclasses().iter().find(|c| c.oid == oid)
}

/// The answer of the `null` from `amproperty` for an `INCLUDE` column of `gist` and `spgist`.
fn no_opclass(index: &Index, attno: i16, prop: &str) -> bool {
    matches!(
        (index.am, prop),
        (GIST, "returnable" | "distance_orderable") | (SPGIST, "distance_orderable")
    ) && opclass(index, attno).is_none()
}

/// `index_can_return` for an access method that has `amcanreturn` and no answer in `amproperty`: `spgcanreturn` of `spgist`. An `INCLUDE` column can always give its data. The operator class `poly_ops` stores a box and not the polygon, so its `config` function sets `canReturnData` to false.
fn can_return(index: &Index, attno: i16) -> bool {
    if attno > 1 {
        return true;
    }
    opclass(index, attno).is_some_and(|c| c.name != "poly_ops")
}

/// The property `prop`, in lower case, of the column `attno` of the index.
fn column_property(index: &Index, routine: &Routine, attno: i16, prop: &str) -> Option<bool> {
    if no_opclass(index, attno, prop) {
        return None;
    }
    if let Some(answer) = am_property(index, attno, prop) {
        return answer;
    }
    let key = !(routine.can_include && attno > index.keys);
    let option = |mask: i16, expect: i16| {
        let value = index.options.get(usize::try_from(attno - 1).ok()?).copied()?;
        Some(routine.can_order && value & mask == expect)
    };
    match prop {
        "asc" if key => option(DESC, 0),
        "desc" if key => option(DESC, DESC),
        "nulls_first" if key => option(NULLS_FIRST, NULLS_FIRST),
        "nulls_last" if key => option(NULLS_FIRST, 0),
        "orderable" => Some(key && routine.can_order),
        "distance_orderable" if !key || !routine.can_order_by_op => Some(false),
        "returnable" => Some(routine.can_return && can_return(index, attno)),
        "search_array" if key => Some(routine.search_array),
        "search_nulls" if key => Some(routine.search_nulls),
        _ => None,
    }
}

/// `indexam_property`: the property `name` of the access method `am`, of the index `index_oid` when it is not 0, and of its column `attno` when it is not 0.
fn property(
    name: &str,
    mut am: u32,
    index_oid: u32,
    attno: i32,
    session: &dyn Session,
) -> Option<bool> {
    let prop = name.to_ascii_lowercase();
    let mut natts = 0;
    let mut found = None;
    if index_oid != 0 {
        let index = index(index_oid, session)?;
        am = index.am;
        natts = index.natts;
        found = Some(index);
    }
    if attno < 0 || attno > i32::from(natts) {
        return None;
    }
    let routine = routine(am)?;
    let attno = i16::try_from(attno).ok()?;
    if let Some(index) = &found
        && attno > 0
    {
        return column_property(index, &routine, attno, &prop);
    }
    if found.is_some() {
        return match prop.as_str() {
            "clusterable" => Some(routine.clusterable),
            "index_scan" => Some(routine.get_tuple),
            "bitmap_scan" => Some(true),
            "backward_scan" => Some(routine.can_backward),
            _ => None,
        };
    }
    match prop.as_str() {
        "can_order" => Some(routine.can_order),
        "can_unique" => Some(routine.can_unique),
        "can_multi_col" => Some(routine.can_multi_col),
        "can_exclude" => Some(routine.get_tuple),
        "can_include" => Some(routine.can_include),
        _ => None,
    }
}

/// A `bool` or null.
fn answer(value: Option<bool>) -> Value {
    value.map_or(Value::Null, Value::Bool)
}

/// The OID argument at `at`.
fn oid_arg(args: &[Value], at: usize) -> Result<u32> {
    args.get(at).and_then(Value::as_oid).ok_or_else(bad_value)
}

/// The name of the property at `at`.
fn name_arg(args: &[Value], at: usize) -> Result<&str> {
    args.get(at).and_then(Value::as_str).ok_or_else(bad_value)
}

/// `pg_indexam_has_property(oid, text)`.
fn pg_indexam_has_property(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let am = oid_arg(args, 0)?;
    Ok(answer(property(name_arg(args, 1)?, am, 0, 0, call.session)))
}

/// `pg_index_has_property(regclass, text)`.
fn pg_index_has_property(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let index = oid_arg(args, 0)?;
    Ok(answer(property(name_arg(args, 1)?, 0, index, 0, call.session)))
}

/// `pg_index_column_has_property(regclass, integer, text)`.
fn pg_index_column_has_property(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let index = oid_arg(args, 0)?;
    let Some(Value::Int4(attno)) = args.get(1) else { return Err(bad_value()) };
    // Column 0 is not a column, and `property` reads it as a property of the index.
    if *attno <= 0 {
        return Ok(Value::Null);
    }
    Ok(answer(property(name_arg(args, 2)?, 0, index, *attno, call.session)))
}

/// `BRIN_AM_OID`.
const BRIN: u32 = 3580;

/// `BRIN_ALL_BLOCKRANGES`: the block number of `brin_summarize_range` that summarizes all the ranges.
const ALL_BLOCK_RANGES: i64 = 0xFFFF_FFFF;

/// `ShareUpdateExclusiveLock`.
const SHARE_UPDATE_EXCLUSIVE: u8 = 4;

/// The name, the kind, the access method and the table of the relation `oid`, from the catalog of the session or from the built-in catalogs. A built-in index is a `btree` index.
fn relation_of(oid: u32, session: &dyn Session) -> Option<(String, RelKind, u32, u32)> {
    if let Some(rel) = session.catalog().and_then(|c| c.relation(oid)) {
        let (method, table) = rel.index.as_ref().map_or((0, 0), |i| (i.method, i.table));
        return Some((rel.name.clone(), rel.kind, method, table));
    }
    let row = builtin::class_by_oid(oid)?;
    let kind = if row.kind == b'i' { RelKind::Index } else { RelKind::Table };
    Some((row.name.to_owned(), kind, BTREE, 0))
}

/// The checks of `brin_summarize_range` and `brin_desummarize_range` for the index `oid`, after the check of the block: `index_open` of an index, a BRIN index, the parent table and `MAINTAIN` on it. The table and the index get `ShareUpdateExclusiveLock`.
fn brin_index(oid: u32, session: &dyn Session) -> Result<()> {
    let Some((name, kind, method, table)) = relation_of(oid, session) else {
        return Err(Error::internal(format!("could not open relation with OID {oid}")));
    };
    if table != 0 {
        session.lock_relations(&[table], SHARE_UPDATE_EXCLUSIVE, None)?;
    }
    if !matches!(kind, RelKind::Index | RelKind::PartitionedIndex) {
        return Err(Error::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{name}\" is not an index")));
    }
    session.lock_relations(&[oid], SHARE_UPDATE_EXCLUSIVE, None)?;
    if kind != RelKind::Index || method != BRIN {
        return Err(Error::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!("\"{name}\" is not a BRIN index"),
        ));
    }
    if table == 0 {
        return Err(Error::new(
            SqlState::UNDEFINED_TABLE,
            format!("could not open parent table of index \"{name}\""),
        ));
    }
    if !crate::acl::relation_allowed(table, crate::acl::ACL_MAINTAIN, session) {
        return Err(Error::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("must be owner of index {name}"),
        ));
    }
    Ok(())
}

/// The error of a block number out of the range of a function.
fn block_out_of_range(block: i64) -> Error {
    Error::new(SqlState::INVALID_PARAMETER_VALUE, format!("block number out of range: {block}"))
}

/// `brin_summarize_range(regclass, bigint)`: the number of the ranges that got a summary, the range of the block, or all the ranges for `BRIN_ALL_BLOCKRANGES`.
fn brin_summarize_range(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let index = oid_arg(args, 0)?;
    let Some(Value::Int8(block)) = args.get(1) else { return Err(bad_value()) };
    if !(0..=ALL_BLOCK_RANGES).contains(block) {
        return Err(block_out_of_range(*block));
    }
    summarize(call, index, (*block != ALL_BLOCK_RANGES).then(|| u32::try_from(*block).unwrap_or(0)))
}

/// `brin_summarize_new_values(regclass)`: `brin_summarize_range` of all the ranges.
fn brin_summarize_new_values(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    summarize(call, oid_arg(args, 0)?, None)
}

/// `brinsummarize` of the range of `block`, or of all the ranges for `None`, of a valid index.
fn summarize(call: &Call<'_>, index: u32, block: Option<u32>) -> Result<Value> {
    brin_index(index, call.session)?;
    let valid = call
        .session
        .catalog()
        .and_then(|c| c.relation(index))
        .and_then(|r| r.index.as_ref())
        .is_some_and(|i| i.valid);
    let count =
        if valid { call.session.brin_ranges(index, BrinRanges::Summarize(block))? } else { 0 };
    Ok(Value::Int4(i32::try_from(count).unwrap_or(i32::MAX)))
}

/// `brin_desummarize_range(regclass, bigint)`: the range of the block loses its summary.
fn brin_desummarize_range(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let index = oid_arg(args, 0)?;
    let Some(Value::Int8(block)) = args.get(1) else { return Err(bad_value()) };
    // `MaxBlockNumber`.
    if !(0..ALL_BLOCK_RANGES).contains(block) {
        return Err(block_out_of_range(*block));
    }
    brin_index(index, call.session)?;
    let block = u32::try_from(*block).unwrap_or(u32::MAX);
    call.session.brin_ranges(index, BrinRanges::Desummarize(block))?;
    Ok(Value::text(""))
}

/// `GIN_AM_OID`.
const GIN: u32 = 2742;

/// `gin_clean_pending_list(regclass)`: the entries of the pending list of a GIN index go to its tree, and the result is the number of the pages of the list that went.
fn gin_clean_pending_list(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = oid_arg(args, 0)?;
    let session = call.session;
    let Some((name, kind, method, table)) = relation_of(oid, session) else {
        return Err(Error::internal(format!("could not open relation with OID {oid}")));
    };
    // `RowExclusiveLock`.
    session.lock_relations(&[oid], 3, None)?;
    if !matches!(kind, RelKind::Index | RelKind::PartitionedIndex) {
        return Err(Error::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{name}\" is not an index")));
    }
    if kind != RelKind::Index || method != GIN {
        return Err(Error::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!("\"{name}\" is not a GIN index"),
        ));
    }
    if table == 0 || !crate::acl::relation_allowed(table, crate::acl::ACL_MAINTAIN, session) {
        return Err(Error::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("must be owner of index {name}"),
        ));
    }
    let pages = session.gin_clean_pending_list(oid)?;
    Ok(Value::Int8(i64::try_from(pages).unwrap_or(i64::MAX)))
}

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    let kernel: Kernel = match src {
        "pg_indexam_has_property" => pg_indexam_has_property,
        "pg_index_has_property" => pg_index_has_property,
        "pg_index_column_has_property" => pg_index_column_has_property,
        "brin_summarize_range" => brin_summarize_range,
        "brin_summarize_new_values" => brin_summarize_new_values,
        "brin_desummarize_range" => brin_desummarize_range,
        "gin_clean_pending_list" => gin_clean_pending_list,
        _ => return None,
    };
    Some(kernel)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An index of the access method `am` with the operator classes `classes` for its key columns and `include` more columns.
    fn index(am: u32, classes: &[&str], include: i16) -> Index {
        let keys = i16::try_from(classes.len()).unwrap();
        let classes = classes
            .iter()
            .map(|name| {
                builtin::opclasses()
                    .iter()
                    .find(|c| c.method == am && c.name == *name)
                    .unwrap_or_else(|| panic!("no class {name}"))
                    .oid
            })
            .collect();
        Index {
            am,
            natts: keys + include,
            keys,
            classes,
            options: vec![0; usize::try_from(keys).unwrap()],
        }
    }

    /// The answers for `distance_orderable` and `returnable` of each column, as `t`, `f` or `-` for null.
    fn answers(index: &Index) -> String {
        let routine = routine(index.am).unwrap();
        let show = |v: Option<bool>| v.map_or('-', |v| if v { 't' } else { 'f' });
        (1..=index.natts)
            .map(|attno| {
                let distance = column_property(index, &routine, attno, "distance_orderable");
                let returnable = column_property(index, &routine, attno, "returnable");
                format!("{}{}", show(distance), show(returnable))
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The answers of PostgreSQL 19 for the columns of gist, spgist, gin and brin indexes. rupg cannot make these indexes yet, so the SQL test does not cover them.
    #[test]
    fn other_methods() {
        let gist = index(GIST, &["box_ops", "range_ops", "poly_ops", "circle_ops", "inet_ops"], 0);
        assert_eq!(answers(&gist), "tt ft tf tf ft");
        assert_eq!(answers(&index(GIST, &["point_ops"], 1)), "tt --");
        assert_eq!(answers(&index(GIST, &["tsvector_ops"], 0)), "ff");
        assert_eq!(answers(&index(SPGIST, &["quad_point_ops"], 1)), "tt -t");
        for (class, expect) in [
            ("kd_point_ops", "tt"),
            ("text_ops", "ft"),
            ("inet_ops", "ft"),
            ("range_ops", "ft"),
            ("box_ops", "tt"),
            ("poly_ops", "tf"),
        ] {
            assert_eq!(answers(&index(SPGIST, &[class], 0)), expect, "{class}");
        }
        assert_eq!(answers(&index(2742, &["tsvector_ops", "jsonb_ops"], 0)), "ff ff");
        assert_eq!(answers(&index(3580, &["int4_minmax_ops", "box_inclusion_ops"], 0)), "ff ff");
        let gin = index(2742, &["tsvector_ops"], 0);
        let routine = routine(2742).unwrap();
        assert_eq!(column_property(&gin, &routine, 1, "asc"), Some(false));
        assert_eq!(column_property(&gin, &routine, 1, "search_nulls"), Some(false));
        let sq = index(SPGIST, &["quad_point_ops"], 1);
        let routine = super::routine(SPGIST).unwrap();
        assert_eq!(column_property(&sq, &routine, 2, "orderable"), Some(false));
        assert_eq!(column_property(&sq, &routine, 2, "asc"), None);
    }
}
