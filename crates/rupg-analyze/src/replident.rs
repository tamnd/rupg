//! `CheckCmdReplicaIdentity` of `execReplication.c`, with `RelationBuildPublicationDesc` of `relcache.c` and `pub_rf_contains_invalid_column` and `pub_contains_invalid_column` of `publicationcmds.c`: an `UPDATE` or a `DELETE` of a table that a publication publishes needs a replica identity that the row filters and the column lists of the publications can use.

use rupg_catalog::{Catalog, RelKind, Relation};
use rupg_common::{Error, Result, SqlState};

use crate::expr::ExprKind;
use crate::insert::WriteKind;

/// `PublicationDesc`: the actions that the publications of a table publish, and whether its replica identity is valid for them.
struct PublicationDesc {
    update: bool,
    delete: bool,
    rf_valid_for_update: bool,
    rf_valid_for_delete: bool,
    cols_valid_for_update: bool,
    cols_valid_for_delete: bool,
    gencols_valid_for_update: bool,
    gencols_valid_for_delete: bool,
}

/// `relreplident` of the table, with the default of its kind for 0.
fn replica_identity(rel: &Relation) -> u8 {
    match rel.replica_identity {
        0 if matches!(rel.kind, RelKind::Table | RelKind::Partitioned | RelKind::MatView) => b'd',
        0 => b'n',
        code => code,
    }
}

/// `RelationGetReplicaIndex` with `RelationGetIndexAttrBitmap` for `INDEX_ATTR_BITMAP_IDENTITY_KEY`: the key columns of the replica identity index of the table, or `None` when the table has no such index. The index is the primary key for `REPLICA IDENTITY DEFAULT` and the chosen index for `REPLICA IDENTITY USING INDEX`. `RelationGetIndexList` skips an index that is not valid, not unique, not immediate or partial.
fn identity_key(catalog: &Catalog, rel: &Relation) -> Option<Vec<i16>> {
    let code = replica_identity(rel);
    catalog.relations().find_map(|r| {
        let info = r.index.as_ref()?;
        let usable = info.table == rel.oid
            && info.valid
            && info.unique
            && info.immediate
            && info.predicate.is_none();
        let chosen = match code {
            b'd' => info.primary,
            b'i' => info.replica_identity,
            _ => false,
        };
        (usable && chosen).then(|| {
            let count = usize::try_from(info.key_count).unwrap_or_default();
            info.keys.iter().take(count).copied().filter(|&k| k > 0).collect()
        })
    })
}

/// `GetRelationIncludedPublications` with `except` false, or `GetRelationExcludedPublications` with `except` true.
fn relation_publications(catalog: &Catalog, relation: u32, except: bool) -> Vec<u32> {
    catalog
        .publication_rels()
        .filter(|r| r.relation == relation && r.except == except)
        .map(|r| r.publication)
        .collect()
}

/// `GetSchemaPublications`.
fn schema_publications(catalog: &Catalog, schema: u32) -> Vec<u32> {
    catalog.publication_namespaces().filter(|n| n.schema == schema).map(|n| n.publication).collect()
}

/// `list_concat_unique_oid`.
fn add_unique(list: &mut Vec<u32>, more: Vec<u32>) {
    for oid in more {
        if !list.contains(&oid) {
            list.push(oid);
        }
    }
}

/// `GetTopMostAncestorInPublication`: the last of the ancestors that the publication has, by itself or by its schema, or `None`.
fn top_ancestor_in(catalog: &Catalog, publication: u32, ancestors: &[u32]) -> Option<u32> {
    ancestors.iter().copied().rev().find(|&a| {
        relation_publications(catalog, a, false).contains(&publication)
            || catalog
                .relation(a)
                .is_some_and(|r| schema_publications(catalog, r.namespace).contains(&publication))
    })
}

/// The number of the column of `to` with the name of the column `attnum` of `from`, as `get_attname` and `get_attnum` give it, or 0.
fn same_column(catalog: &Catalog, from: u32, attnum: i16, to: u32) -> i16 {
    let name = catalog
        .relation(from)
        .and_then(|r| r.columns.get(usize::try_from(attnum - 1).ok()?))
        .map(|c| c.name.clone());
    let Some(name) = name else { return 0 };
    catalog
        .relation(to)
        .and_then(|r| r.columns.iter().position(|c| !c.dropped && c.name == name))
        .and_then(|i| i16::try_from(i + 1).ok())
        .unwrap_or_default()
}

/// The table whose row filter and column list the publication uses for the table: the topmost ancestor that the publication has, for a partition of a publication with `publish_via_partition_root`, or else the table.
fn publish_as(
    catalog: &Catalog,
    publication: u32,
    rel: &Relation,
    ancestors: &[u32],
    via_root: bool,
) -> u32 {
    if via_root && rel.partition_of.is_some() {
        return top_ancestor_in(catalog, publication, ancestors).unwrap_or(rel.oid);
    }
    rel.oid
}

/// `pub_rf_contains_invalid_column`: true when the row filter of the publication for the table has a column that is not in the replica identity.
fn row_filter_invalid(
    catalog: &Catalog,
    publication: u32,
    rel: &Relation,
    ancestors: &[u32],
    via_root: bool,
) -> bool {
    if replica_identity(rel) == b'f' {
        return false;
    }
    let source = publish_as(catalog, publication, rel, ancestors, via_root);
    let Some(text) = catalog.publication_rel_of(publication, source).and_then(|r| r.qual.clone())
    else {
        return false;
    };
    let Ok(qual) = crate::node::read(&text) else { return false };
    let key = identity_key(catalog, rel).unwrap_or_default();
    qual.find(0, &mut |e, _| {
        let ExprKind::Var(var) = &e.kind else { return None };
        let attnum =
            if via_root { same_column(catalog, source, var.attnum, rel.oid) } else { var.attnum };
        (!key.contains(&attnum)).then_some(())
    })
    .is_some()
}

/// `pub_contains_invalid_column`: whether the column list of the publication for the table does not cover the replica identity, and whether the replica identity has a generated column that the publication does not publish.
fn columns_invalid(
    catalog: &Catalog,
    publication: u32,
    rel: &Relation,
    ancestors: &[u32],
    (via_root, all_tables, gencols): (bool, bool, u8),
) -> (bool, bool) {
    let source = publish_as(catalog, publication, rel, ancestors, via_root);
    // `check_and_fetch_column_list`: a publication `FOR ALL TABLES` has no column lists.
    let columns = if all_tables {
        None
    } else {
        catalog.publication_rel_of(publication, source).and_then(|r| r.attrs.clone())
    };
    let mut invalid_list = false;
    let mut invalid_gen = false;
    let live = || rel.columns.iter().filter(|c| !c.dropped);
    if replica_identity(rel) == b'f' {
        // With `REPLICA IDENTITY FULL` no column list is allowed, and the publication must publish the stored generated columns. Virtual generated columns cannot be published.
        invalid_list = columns.is_some();
        if gencols != b's' && live().any(|c| c.generated == b's') {
            invalid_gen = true;
        }
        if live().any(|c| c.generated == b'v') {
            invalid_gen = true;
        }
        if invalid_list && invalid_gen {
            return (true, true);
        }
    }
    for attnum in identity_key(catalog, rel).unwrap_or_default() {
        let generated = usize::try_from(attnum - 1)
            .ok()
            .and_then(|i| rel.columns.get(i))
            .map_or(0, |c| c.generated);
        let Some(columns) = &columns else {
            if (generated == b's' && gencols != b's') || generated == b'v' {
                invalid_gen = true;
                break;
            }
            continue;
        };
        let attnum = if via_root { same_column(catalog, rel.oid, attnum, source) } else { attnum };
        invalid_list |= !columns.contains(&attnum);
        if invalid_list && invalid_gen {
            break;
        }
    }
    (invalid_list, invalid_gen)
}

/// `RelationBuildPublicationDesc`: the actions that the publications of the table publish, with the publications of its schema, of its ancestors and the publications `FOR ALL TABLES` that do not exclude its topmost ancestor.
fn publication_desc(catalog: &Catalog, rel: &Relation) -> PublicationDesc {
    let mut desc = PublicationDesc {
        update: false,
        delete: false,
        rf_valid_for_update: true,
        rf_valid_for_delete: true,
        cols_valid_for_update: true,
        cols_valid_for_delete: true,
        gencols_valid_for_update: true,
        gencols_valid_for_delete: true,
    };
    if !catalog.is_publishable(rel) {
        return desc;
    }
    let mut publications = relation_publications(catalog, rel.oid, false);
    add_unique(&mut publications, schema_publications(catalog, rel.namespace));
    let ancestors =
        if rel.partition_of.is_some() { catalog.partition_ancestors(rel.oid) } else { Vec::new() };
    for &ancestor in &ancestors {
        add_unique(&mut publications, relation_publications(catalog, ancestor, false));
        let schema = catalog.relation(ancestor).map_or(0, |r| r.namespace);
        add_unique(&mut publications, schema_publications(catalog, schema));
    }
    // Only the topmost ancestor can be in an `EXCEPT` clause.
    let excluded =
        relation_publications(catalog, ancestors.last().copied().unwrap_or(rel.oid), true);
    add_unique(
        &mut publications,
        catalog
            .publications()
            .filter(|p| p.all_tables && !excluded.contains(&p.oid))
            .map(|p| p.oid)
            .collect(),
    );
    let (mut insert, mut truncate) = (false, false);
    for oid in publications {
        let Some(publication) = catalog.publication(oid) else { continue };
        insert |= publication.insert;
        desc.update |= publication.update;
        desc.delete |= publication.delete;
        truncate |= publication.truncate;
        let changes = publication.update || publication.delete;
        if !publication.all_tables
            && changes
            && row_filter_invalid(catalog, oid, rel, &ancestors, publication.via_root)
        {
            if publication.update {
                desc.rf_valid_for_update = false;
            }
            if publication.delete {
                desc.rf_valid_for_delete = false;
            }
        }
        if changes {
            let options = (publication.via_root, publication.all_tables, publication.gencols);
            let (list, generated) = columns_invalid(catalog, oid, rel, &ancestors, options);
            if list || generated {
                if publication.update {
                    desc.cols_valid_for_update = !list;
                    desc.gencols_valid_for_update = !generated;
                }
                if publication.delete {
                    desc.cols_valid_for_delete = !list;
                    desc.gencols_valid_for_delete = !generated;
                }
            }
        }
        // With all the actions published and the checks failed for both `UPDATE` and `DELETE`, the other publications cannot change the result.
        let all = insert && desc.update && desc.delete && truncate;
        if all
            && ((!desc.rf_valid_for_update && !desc.rf_valid_for_delete)
                || (!desc.cols_valid_for_update && !desc.cols_valid_for_delete)
                || (!desc.gencols_valid_for_update && !desc.gencols_valid_for_delete))
        {
            break;
        }
    }
    desc
}

/// `CheckCmdReplicaIdentity`: the error of an `UPDATE` or a `DELETE` of the table when a publication publishes the action and the replica identity of the table does not allow it. A partitioned table has no check, as its leaf partitions do the writes, and an `INSERT` has none.
///
/// # Errors
///
/// `42P10` when a row filter or a column list of a publication does not fit the replica identity, and `55000` when the table has no replica identity.
pub fn check_cmd_replica_identity(catalog: &Catalog, table: u32, kind: WriteKind) -> Result<()> {
    let Some(rel) = catalog.relation(table) else { return Ok(()) };
    if rel.kind != RelKind::Table || !matches!(kind, WriteKind::Update | WriteKind::Delete) {
        return Ok(());
    }
    let desc = publication_desc(catalog, rel);
    let name = &rel.name;
    let update = kind == WriteKind::Update;
    let what = if update {
        format!("cannot update table \"{name}\"")
    } else {
        format!("cannot delete from table \"{name}\"")
    };
    let detail = if update {
        [desc.rf_valid_for_update, desc.cols_valid_for_update, desc.gencols_valid_for_update]
    } else {
        [desc.rf_valid_for_delete, desc.cols_valid_for_delete, desc.gencols_valid_for_delete]
    }
    .iter()
    .zip([
        "Column used in the publication WHERE expression is not part of the replica identity.",
        "Column list used by the publication does not cover the replica identity.",
        "Replica identity must not contain unpublished generated columns.",
    ])
    .find_map(|(valid, detail)| (!valid).then_some(detail));
    if let Some(detail) = detail {
        return Err(Error::new(SqlState::INVALID_COLUMN_REFERENCE, what).with_detail(detail));
    }
    if identity_key(catalog, rel).is_some() || replica_identity(rel) == b'f' {
        return Ok(());
    }
    if update && desc.update {
        return Err(Error::new(
            SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
            format!("{what} because it does not have a replica identity and publishes updates"),
        )
        .with_hint("To enable updating the table, set REPLICA IDENTITY using ALTER TABLE."));
    }
    if !update && desc.delete {
        return Err(Error::new(
            SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
            format!("{what} because it does not have a replica identity and publishes deletes"),
        )
        .with_hint("To enable deleting from the table, set REPLICA IDENTITY using ALTER TABLE."));
    }
    Ok(())
}
