//! `rupg.rupg_link_check(regclass)`, the full check of the links of spec/13 section 13.2.5. A forward link holds the parent version of each child version that a join read, and the join checks each pair, so a link of a DDL index is a validated link of section 13.2.4. The check recomputes the forward value of each child row that the snapshot sees by its parent key, compares it with the pair of the link, and repairs the pair. A validated link stays validated, as a DDL link has no constraint that keeps it trusted.

use std::cmp::Ordering;
use std::sync::PoisonError;

use rupg_analyze::LinkDefinition;
use rupg_catalog::{Catalog, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_types::Value;

use crate::{Call, Kernel, bad_value, compare};

/// The key of a row: the values of the key columns, with the ID of the version, or `None` for a row with a null key, which joins no row.
fn key_of(row: &[Value]) -> Option<(Vec<Value>, u64)> {
    let (version, values) = row.split_last()?;
    let Value::Int8(version) = version else { return None };
    if values.contains(&Value::Null) {
        return None;
    }
    Some((values.to_vec(), version.cast_unsigned()))
}

/// The order of two keys whose columns have the types `types`.
fn order(types: &[u32], a: &[Value], b: &[Value]) -> Result<Ordering> {
    for ((&ty, a), b) in types.iter().zip(a).zip(b) {
        match compare(ty, a, ty, b)? {
            Ordering::Equal => {}
            other => return Ok(other),
        }
    }
    Ok(Ordering::Equal)
}

/// The rows of `table` that the snapshot sees, with the values of only the columns `columns` in their order and then the ID of the version.
fn rows(
    call: &Call<'_>,
    catalog: &Catalog,
    table: u32,
    columns: &[i16],
) -> Result<Vec<Vec<Value>>> {
    let width = catalog.relation(table).map_or(0, |r| r.columns.len());
    let mut used = vec![false; width];
    let mut places = Vec::with_capacity(columns.len());
    for &column in columns {
        let place = usize::try_from(column - 1).map_err(|_| bad_value())?;
        *used.get_mut(place).ok_or_else(bad_value)? = true;
        places.push(place);
    }
    // The values come in the order of the columns in the table, so each key column goes to its place in the link.
    let mut sorted = places.clone();
    sorted.sort_unstable();
    let at: Vec<usize> = places.iter().map(|p| sorted.binary_search(p).unwrap_or(0)).collect();
    let rows = call.session.table_rows(table, Some(&used))?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let (version, values) =
                row.split_last().map_or((Value::Null, &[][..]), |(v, rest)| (v.clone(), rest));
            let mut out: Vec<Value> =
                at.iter().map(|&i| values.get(i).cloned().unwrap_or(Value::Null)).collect();
            out.push(version);
            out
        })
        .collect())
}

/// Checks the link `link` and gives the number of the pairs of its forward link that the check wrote or took out.
fn check(call: &Call<'_>, catalog: &Catalog, link: &LinkDefinition) -> Result<i64> {
    let Some(forward) = call.session.link_forward(link.index) else { return Ok(0) };
    let child = catalog.relation(link.child).ok_or_else(bad_value)?;
    let types: Vec<u32> = link.keys.iter().map(|&k| child.column(k).map_or(0, |c| c.ty)).collect();
    // The parents by their keys. The key of the parent is unique, so the snapshot sees one version at most for each key.
    let mut parents: Vec<(Vec<Value>, u64)> = Vec::new();
    for row in rows(call, catalog, link.parent, &link.parent_keys)? {
        parents.extend(key_of(&row));
    }
    let mut failed = None;
    parents.sort_by(|a, b| {
        order(&types, &a.0, &b.0).unwrap_or_else(|e| {
            failed.get_or_insert(e);
            Ordering::Equal
        })
    });
    if let Some(error) = failed {
        return Err(error);
    }
    let children = rows(call, catalog, link.child, &link.keys)?;
    let mut pairs = forward.lock().unwrap_or_else(PoisonError::into_inner);
    let mut repaired = 0;
    for row in &children {
        let Some(Value::Int8(version)) = row.last() else { continue };
        let version = version.cast_unsigned();
        let parent = match key_of(row) {
            Some((key, _)) => {
                let mut found = None;
                let (mut low, mut high) = (0, parents.len());
                while low < high {
                    let mid = low + (high - low) / 2;
                    match order(&types, &parents[mid].0, &key)? {
                        Ordering::Less => low = mid + 1,
                        Ordering::Greater => high = mid,
                        Ordering::Equal => {
                            found = Some(parents[mid].1);
                            break;
                        }
                    }
                }
                found
            }
            None => None,
        };
        let changed = match parent {
            Some(parent) => pairs.insert(version, parent) != Some(parent),
            None => pairs.remove(&version).is_some(),
        };
        repaired += i64::from(changed);
    }
    Ok(repaired)
}

/// `rupg.rupg_link_check(regclass)`: the full check of a link index, or of each link index of a table, with the number of the pairs that it repaired. A link index that a join cannot use, as [`rupg_analyze::link_definition`] finds it, has no pairs to check.
fn link_check(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = args.first().and_then(Value::as_oid).ok_or_else(bad_value)?;
    let catalog = call.session.catalog().ok_or_else(bad_value)?;
    let relation = catalog.relation(oid).ok_or_else(|| {
        Error::new(SqlState::UNDEFINED_TABLE, format!("relation with OID {oid} does not exist"))
    })?;
    let links: Vec<LinkDefinition> = match (&relation.index, relation.kind) {
        (Some(index), _) if index.method == rupg_index::am::LINK => {
            rupg_analyze::link_definition(catalog, oid).into_iter().collect()
        }
        (None, RelKind::Table | RelKind::Partitioned) => rupg_analyze::links_of(catalog, oid),
        _ => {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{}\" is not a table or a link index", relation.name),
            ));
        }
    };
    let mut repaired = 0;
    for link in &links {
        repaired += check(call, catalog, link)?;
    }
    Ok(Value::Int8(repaired))
}

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    match src {
        "rupg_link_check" => Some(link_check),
        _ => None,
    }
}
