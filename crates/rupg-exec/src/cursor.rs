//! The plan of a cursor that `WHERE CURRENT OF` can read, as `execCurrentOf` finds the row of a table in the plan of the cursor.

use std::sync::Arc;

use rupg_analyze::{Expr, ExprKind, FromItem, Query, Relation, TABLE_OID_ATTNUM, Target, Var};
use rupg_types::oid;

use crate::Plan;

/// `SelfItemPointerAttributeNumber`.
const CTID_ATTNUM: i16 = -1;

impl Plan {
    /// The plan of a cursor. After its columns, the query also gives the `tableoid` and the `ctid` of the row of each table that `execCurrentOf` can find in it. A query with a locking clause gives them for each table with a row mark, as the `ExecRowMark` of `FOR UPDATE` keeps them. Another query gives them for its table when it is a scan of one table, as `search_plan_tree` finds the scan, also below a subquery of `FROM`.
    #[must_use]
    pub fn cursor(&self) -> Plan {
        let mut query = self.query.clone();
        let written = Arc::clone(&self.written);
        let marks = if query.row_marks.is_empty() {
            scan(&mut query, false).into_iter().collect()
        } else {
            let marks: Vec<(usize, u32)> = query
                .row_marks
                .iter()
                .filter_map(|m| {
                    query
                        .relations
                        .get(m.relation)
                        .filter(|r| plain(r))
                        .map(|r| (m.relation, r.oid))
                })
                .collect();
            let added = marks.iter().flat_map(|&(i, _)| {
                [var(i, TABLE_OID_ATTNUM, oid::OID), var(i, CTID_ATTNUM, oid::INT8)]
            });
            add(&mut query, added.collect());
            marks.into_iter().map(|(_, oid)| oid).collect()
        };
        Plan { query, written, marks, run: None }
    }

    /// The OID of each table whose `tableoid` and `ctid` the rows of the cursor end with, from [`Plan::cursor`].
    pub fn marks(&self) -> &[u32] {
        &self.marks
    }
}

/// A target that is a system column of the relation `relation`.
fn var(relation: usize, attnum: i16, ty: u32) -> Target {
    Target {
        name: String::new(),
        expr: Expr::new(ExprKind::Var(Var { relation, attnum, levels_up: 0 }), ty),
        origin: None,
        junk: false,
    }
}

/// True for a relation that is a table.
fn plain(r: &Relation) -> bool {
    r.oid != 0
        && r.subquery.is_none()
        && r.function.is_none()
        && r.values.is_none()
        && r.cte.is_none()
}

/// Adds the targets `added` after the columns of the query.
fn add(query: &mut Query, added: Vec<Target>) {
    let at = query.targets.iter().take_while(|t| !t.junk).count();
    let shift = added.len();
    query.targets.splice(at..at, added);
    for sort in query.sort.iter_mut().chain(&mut query.group).chain(&mut query.distinct) {
        if sort.target >= at {
            sort.target += shift;
        }
    }
}

/// `search_plan_tree`: for a query that is a scan of one table, or of a subquery of `FROM` that is such a scan, adds the `tableoid` and the `ctid` of the row of the table after the columns and gives the OID of the table. A subquery gives the `ctid` first, as the row of a table ends with it, so that the query above reads the two columns of the subquery as those of a table.
fn scan(query: &mut Query, sub: bool) -> Option<u32> {
    if !simple(query) {
        return None;
    }
    let &[FromItem::Relation(i)] = query.from.as_slice() else { return None };
    let r = query.relations.get_mut(i)?;
    let oid = if plain(r) {
        r.oid
    } else if r.function.is_none() && r.values.is_none() && r.cte.is_none() {
        scan(r.subquery.as_deref_mut()?, true)?
    } else {
        return None;
    };
    let (tableoid, ctid) = (var(i, TABLE_OID_ATTNUM, oid::OID), var(i, CTID_ATTNUM, oid::INT8));
    add(query, if sub { vec![ctid, tableoid] } else { vec![tableoid, ctid] });
    Some(oid)
}

/// True for a query whose plan is a scan of its relation with no node above it that hides the row of the relation, such as a sort or an aggregate.
fn simple(query: &Query) -> bool {
    !query.grouped
        && query.group.is_empty()
        && query.having.is_none()
        && query.sort.is_empty()
        && query.distinct.is_empty()
        && query.set_op.is_none()
        && query.windows.is_empty()
}
