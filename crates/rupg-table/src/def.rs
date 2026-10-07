//! The definition of a table: its columns and their types, and the map from typed values to the storage form of the hot store.

use rupg_common::{Error, Oid, Result, SqlState};
use rupg_hot::{Column, Schema, Width};
use rupg_types::{Datum, TypeId};

/// One column of a table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnDef {
    /// The name.
    pub name: String,
    /// The type.
    pub ty: TypeId,
    /// True if a value can be NULL.
    pub nullable: bool,
}

/// A table definition. It is checked when it is made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableDef {
    oid: Oid,
    name: String,
    columns: Vec<ColumnDef>,
    schema: Schema,
}

impl TableDef {
    /// A table definition. A repeated column name gives SQLSTATE `42701`. More than 1,600 columns give `54011`. A type that M1 does not store gives `0A000`.
    pub fn new(oid: Oid, name: impl Into<String>, columns: Vec<ColumnDef>) -> Result<TableDef> {
        for (i, c) in columns.iter().enumerate() {
            if columns[..i].iter().any(|d| d.name == c.name) {
                return Err(Error::new(
                    SqlState::DUPLICATE_COLUMN,
                    format!("column \"{}\" specified more than once", c.name),
                ));
            }
            if TypeId::from_oid(c.ty.0).is_none() {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("the type with OID {} is not supported yet", c.ty.0),
                ));
            }
        }
        let storage = columns
            .iter()
            .map(|c| Column {
                width: c.ty.width().map_or(Width::Variable, Width::Fixed),
                nullable: c.nullable,
            })
            .collect();
        let schema = Schema::new(1, storage)?;
        Ok(TableDef { oid, name: name.into(), columns, schema })
    }

    /// The OID of the table.
    pub fn oid(&self) -> Oid {
        self.oid
    }

    /// The name of the table.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The columns, in order.
    pub fn columns(&self) -> &[ColumnDef] {
        &self.columns
    }

    /// The storage form of the columns for the hot store.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The position of the column `name`, or `None`.
    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// The storage form of a row. The values must be one for each column, in order. A value count that differs gives SQLSTATE `42601`. A value of another type gives `42804`. A NULL in a column that is not nullable gives `23502`. The messages are those of PostgreSQL for an `INSERT`.
    pub fn encode(&self, values: &[Datum]) -> Result<Vec<Option<Vec<u8>>>> {
        if values.len() != self.columns.len() {
            let more = if values.len() > self.columns.len() {
                "more expressions than target columns"
            } else {
                "more target columns than expressions"
            };
            return Err(Error::new(SqlState::SYNTAX_ERROR, format!("INSERT has {more}")));
        }
        self.columns
            .iter()
            .zip(values)
            .map(|(c, v)| match v.type_id() {
                None if c.nullable => Ok(None),
                None => Err(Error::new(
                    SqlState::NOT_NULL_VIOLATION,
                    format!(
                        "null value in column \"{}\" of relation \"{}\" violates not-null constraint",
                        c.name, self.name
                    ),
                )),
                Some(ty) if ty == c.ty => Ok(v.encode()),
                Some(ty) => Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!(
                        "column \"{}\" is of type {} but expression is of type {}",
                        c.name,
                        c.ty.sql_name(),
                        ty.sql_name()
                    ),
                )),
            })
            .collect()
    }

    /// The values of a row in storage form. A bad storage form gives SQLSTATE `XX001`.
    pub fn decode(&self, stored: &[Option<Vec<u8>>]) -> Result<Vec<Datum>> {
        if stored.len() != self.columns.len() {
            return Err(Error::corrupted(format!(
                "a row of the table \"{}\" has {} values for {} columns",
                self.name,
                stored.len(),
                self.columns.len()
            )));
        }
        self.columns.iter().zip(stored).map(|(c, v)| Datum::decode(c.ty, v.as_deref())).collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn def() -> TableDef {
        let col = |name: &str, ty, nullable| ColumnDef { name: name.into(), ty, nullable };
        let columns = vec![
            col("id", TypeId::INT8, false),
            col("name", TypeId::TEXT, true),
            col("ok", TypeId::BOOL, true),
        ];
        TableDef::new(Oid(16384), "t", columns).unwrap()
    }

    #[test]
    fn values() {
        let def = def();
        assert_eq!(
            (def.oid(), def.name(), def.column("ok"), def.column("x")),
            (Oid(16384), "t", Some(2), None)
        );
        let widths: Vec<Width> = def.schema().columns.iter().map(|c| c.width).collect();
        assert_eq!(widths, [Width::Fixed(8), Width::Variable, Width::Fixed(1)]);
        let row = [Datum::Int8(5), Datum::Null, Datum::Bool(true)];
        let stored = def.encode(&row).unwrap();
        assert_eq!(stored, [Some(5i64.to_le_bytes().to_vec()), None, Some(vec![1])]);
        assert_eq!(def.decode(&stored).unwrap(), row);
        assert_eq!(def.decode(&stored[..2]).unwrap_err().state(), SqlState::DATA_CORRUPTED);
    }

    #[test]
    fn errors() {
        let def = def();
        let err = def.encode(&[Datum::Null, Datum::Null, Datum::Null]).unwrap_err();
        assert_eq!(err.state(), SqlState::NOT_NULL_VIOLATION);
        assert_eq!(
            err.message(),
            "null value in column \"id\" of relation \"t\" violates not-null constraint"
        );
        let err = def.encode(&[Datum::Int4(1), Datum::Null, Datum::Null]).unwrap_err();
        assert_eq!(err.state(), SqlState::DATATYPE_MISMATCH);
        assert_eq!(
            err.message(),
            "column \"id\" is of type bigint but expression is of type integer"
        );
        let err = def.encode(&[Datum::Int8(1)]).unwrap_err();
        assert_eq!(
            (err.state(), err.message()),
            (SqlState::SYNTAX_ERROR, "INSERT has more target columns than expressions")
        );
        let err = def.encode(&[Datum::Null, Datum::Null, Datum::Null, Datum::Null]).unwrap_err();
        assert_eq!(err.message(), "INSERT has more expressions than target columns");

        let col = |name: &str, ty| ColumnDef { name: name.into(), ty, nullable: true };
        let err = TableDef::new(Oid(1), "u", vec![col("a", TypeId::INT4), col("a", TypeId::TEXT)])
            .unwrap_err();
        assert_eq!(
            (err.state(), err.message()),
            (SqlState::DUPLICATE_COLUMN, "column \"a\" specified more than once")
        );
        let err = TableDef::new(Oid(1), "u", vec![col("a", TypeId(Oid(1700)))]).unwrap_err();
        assert_eq!(err.state(), SqlState::FEATURE_NOT_SUPPORTED);
        let many = (0..1_601).map(|i| col(&format!("c{i}"), TypeId::INT4)).collect();
        assert_eq!(
            TableDef::new(Oid(1), "u", many).unwrap_err().state(),
            SqlState::TOO_MANY_COLUMNS
        );
        TableDef::new(Oid(1), "u", Vec::new()).unwrap();
    }
}
