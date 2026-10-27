//! `pg_get_publication_tables` and `pg_get_publication_sequences` of `pg_publication.c`, which read the publications of the catalog.

use rupg_catalog::{Catalog, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_types::{Array, Value};

use crate::{Call, bad_value};

/// The error of `GetPublicationByName` for a name that is not a publication.
fn no_publication(name: &str) -> Error {
    Error::new(SqlState::UNDEFINED_OBJECT, format!("publication \"{name}\" does not exist"))
}

/// An `int2vector`.
fn int2_vector(values: &[i16]) -> Value {
    Value::Array(Box::new(Array::vector(values.iter().map(|&v| Some(Value::Int2(v))).collect())))
}

/// The names of the `VARIADIC` array of publication names. A null element is an error.
fn names(arg: Option<&Value>) -> Result<Vec<&str>> {
    let Some(Value::Array(array)) = arg else { return Err(bad_value()) };
    array
        .values
        .iter()
        .map(|v| match v {
            Some(Value::Text(name)) => Ok(&**name),
            Some(_) => Err(bad_value()),
            None => Err(Error::new(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                "null array element not allowed in this context",
            )),
        })
        .collect()
}

/// The rows of `pg_get_publication_tables`: the publication, the table, the column list and the row filter. A table of a `FOR ALL TABLES` publication or of a schema of the publication has no row filter, and with no column list the list has each column that the publication sends.
fn rows(catalog: &Catalog, pairs: Vec<(u32, u32)>) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    for (publication, relation) in pairs {
        let (Some(pubrow), Some(rel)) =
            (catalog.publication(publication), catalog.relation(relation))
        else {
            continue;
        };
        let in_schema = catalog
            .publication_namespaces()
            .any(|n| n.publication == publication && n.schema == rel.namespace);
        let row = if pubrow.all_tables || in_schema {
            None
        } else {
            catalog.publication_rel_of(publication, relation)
        };
        let attrs = match row.and_then(|r| r.attrs.as_deref()) {
            Some(attrs) => int2_vector(attrs),
            None => {
                let numbers: Vec<i16> = rel
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| {
                        !c.dropped
                            && (c.generated == 0 || (c.generated == b's' && pubrow.gencols == b's'))
                    })
                    .map(|(i, _)| i16::try_from(i + 1).unwrap_or(i16::MAX))
                    .collect();
                if numbers.is_empty() { Value::Null } else { int2_vector(&numbers) }
            }
        };
        let qual = row.and_then(|r| r.qual.clone()).map_or(Value::Null, Value::text);
        out.push(vec![Value::Oid(publication), Value::Oid(relation), attrs, qual]);
    }
    out
}

/// `pg_get_publication_tables_a`: the tables of the publications of the `VARIADIC` array. A name that is not a publication is an error.
pub(crate) fn publication_tables(call: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let names = names(args.first())?;
    let Some(catalog) = call.session.catalog() else {
        return match names.first() {
            Some(name) => Err(no_publication(name)),
            None => Ok(Vec::new()),
        };
    };
    let pairs = catalog.publication_tables(&names, None).map_err(|name| no_publication(&name))?;
    Ok(rows(catalog, pairs))
}

/// `pg_get_publication_tables_b`: the rows of the table in the publications of the array. This form skips the names that are not publications. As in `pg_get_publication_tables`, a table that a publication cannot have gives no rows before the check of the names.
pub(crate) fn publication_tables_of(call: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let Some(Value::Oid(target)) = args.get(1) else { return Err(bad_value()) };
    let Some(catalog) = call.session.catalog() else { return Ok(Vec::new()) };
    if !catalog.is_publishable_table(*target) {
        return Ok(Vec::new());
    }
    let names = names(args.first())?;
    let pairs =
        catalog.publication_tables(&names, Some(*target)).map_err(|name| no_publication(&name))?;
    Ok(rows(catalog, pairs))
}

/// `pg_get_publication_sequences`: the sequences of a `FOR ALL SEQUENCES` publication.
pub(crate) fn publication_sequences(call: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let Some(Value::Text(name)) = args.first() else { return Err(bad_value()) };
    let found = call.session.catalog().and_then(|c| Some((c, c.publication_by_name(name)?)));
    let Some((catalog, publication)) = found else { return Err(no_publication(name)) };
    if !publication.all_sequences {
        return Ok(Vec::new());
    }
    let sequences = catalog.all_publication_relations(publication.oid, RelKind::Sequence, false);
    Ok(sequences.into_iter().map(|s| vec![Value::Oid(s)]).collect())
}
