//! `currtid2` of `src/backend/utils/adt/tid.c`: the latest version of a row of a table, or of the table under a view that shows the `ctid` of the table.

use rupg_analyze::ExprKind;
use rupg_catalog::RelKind;
use rupg_common::{Error, Result, SqlState};
use rupg_types::Value;

use crate::{Call, Kernel, acl, bad_value, io, reg};

/// `SelfItemPointerAttributeNumber`.
const CTID_ATTNUM: i16 = -1;

pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "currtid_byrelname" => currtid_byrelname,
        _ => return None,
    })
}

/// `currtid_byrelname`: the relation of a qualified name, as `table_openrv` opens it.
fn currtid_byrelname(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (Some(name), Some(Value::Int8(tid))) = (args.first().and_then(Value::as_str), args.get(1))
    else {
        return Err(bad_value());
    };
    let relid = acl::table_name(name, call.session)?;
    currtid_internal(call, relid, *tid).map(Value::Int8)
}

/// `currtid_internal`: the check of `SELECT`, then the view or the heap of the relation `relid`.
fn currtid_internal(call: &Call<'_>, relid: u32, tid: i64) -> Result<i64> {
    let session = call.session;
    let Some(rel) = session.catalog().and_then(|c| c.relation(relid)) else {
        return Err(Error::internal(format!("could not open relation with OID {relid}")));
    };
    // `validate_relation_kind` of `table_open`.
    let kind = match rel.kind {
        RelKind::Index => Some("indexes"),
        RelKind::PartitionedIndex => Some("partitioned indexes"),
        RelKind::Composite => Some("composite types"),
        _ => None,
    };
    if let Some(kind) = kind {
        return Err(Error::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!("cannot open relation \"{}\"", rel.name),
        )
        .with_detail(format!("This operation is not supported for {kind}.")));
    }
    if !acl::sequence_allowed(relid, acl::SELECT, session)? {
        let object = match rel.kind {
            RelKind::View => "view",
            RelKind::MatView => "materialized view",
            RelKind::Sequence => "sequence",
            RelKind::Foreign => "foreign table",
            _ => "table",
        };
        return Err(Error::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("permission denied for {object} {}", rel.name),
        ));
    }
    match rel.kind {
        RelKind::View => return for_view(call, relid, tid),
        RelKind::Table | RelKind::Toast | RelKind::MatView | RelKind::Sequence => {}
        _ => {
            let schema = reg::namespace_name(rel.namespace, session).unwrap_or_default();
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("cannot look at latest visible tid for relation \"{schema}.{}\"", rel.name),
            ));
        }
    }
    // `heapam_tuple_tid_valid`: a valid offset in a block of the heap.
    let blocks = match rel.kind {
        RelKind::Sequence => 1,
        _ => session.heap_size(relid).map_or(0, |size| size.blocks),
    };
    let (block, offset) = io::tid_parts(tid)?;
    if offset == 0 || block >= blocks {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("tid ({block}, {offset}) is not valid for relation \"{}\"", rel.name),
        ));
    }
    Ok(if rel.kind == RelKind::Sequence { tid } else { session.latest_tid(relid, tid) })
}

/// `currtid_for_view`: the column `ctid` of the view must be the `ctid` of a relation of the query of the view, and the result is the latest version in that relation.
fn for_view(call: &Call<'_>, relid: u32, tid: i64) -> Result<i64> {
    let session = call.session;
    let Some(rel) = session.catalog().and_then(|c| c.relation(relid)) else {
        return Err(Error::internal(format!("could not open relation with OID {relid}")));
    };
    let Some(index) = rel.columns.iter().position(|c| c.name == "ctid") else {
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "currtid cannot handle views with no CTID",
        ));
    };
    if rel.columns[index].ty != rupg_types::oid::TID {
        return Err(Error::new(SqlState::FEATURE_NOT_SUPPORTED, "ctid isn't of type TID"));
    }
    let Some(query) = rupg_analyze::view_query(&reg::SessionEnv(session), relid)? else {
        return Err(Error::internal("the view has no rules"));
    };
    let target = query.targets.iter().filter(|t| !t.junk).nth(index);
    if let Some(target) = target
        && let ExprKind::Var(var) = &target.expr.kind
        && var.levels_up == 0
        && var.attnum == CTID_ATTNUM
        && let Some(base) = query.relations.get(var.relation)
    {
        return currtid_internal(call, base.oid, tid);
    }
    Err(Error::internal("currtid cannot handle this view"))
}
