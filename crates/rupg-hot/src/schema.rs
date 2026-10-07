//! The storage form of the columns of a table, as the hot store sees them.
//!
//! The hot store does not know SQL types. A column is a fixed number of bytes or a variable number of bytes, and it can be NULL or not. `rupg-table` maps each type to one of these forms.

use rupg_common::{Error, Result, SqlState};

use crate::row::{Row, VersionHeader};

/// The width of the values of a column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Width {
    /// Each value has this number of bytes, from 1 to 255.
    Fixed(u8),
    /// Each value has its own length.
    Variable,
}

/// One column in storage form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Column {
    /// The width of the values.
    pub width: Width,
    /// True if a value can be NULL.
    pub nullable: bool,
}

impl Column {
    /// The bytes of the minipage for `rows` rows, without the variable heap.
    pub(crate) fn minipage(&self, rows: usize) -> usize {
        let nulls = if self.nullable { rows.div_ceil(8) } else { 0 };
        nulls
            + match self.width {
                Width::Fixed(w) => usize::from(w) * rows,
                Width::Variable => 2 * (rows + 1),
            }
    }
}

/// The columns of a table in storage form, with the schema version.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Schema {
    /// The schema version. A leaf records the version of its rows.
    pub version: u32,
    /// The columns.
    pub columns: Vec<Column>,
}

impl Schema {
    /// The most columns in a table, as in PostgreSQL.
    pub const MAX_COLUMNS: usize = 1600;

    /// A schema. More than 1,600 columns give SQLSTATE `54011`. A fixed width of 0 gives `XX000`.
    pub fn new(version: u32, columns: Vec<Column>) -> Result<Schema> {
        if columns.len() > Schema::MAX_COLUMNS {
            return Err(Error::new(
                SqlState::TOO_MANY_COLUMNS,
                format!("tables can have at most {} columns", Schema::MAX_COLUMNS),
            ));
        }
        if columns.iter().any(|c| c.width == Width::Fixed(0)) {
            return Err(Error::internal("a fixed width column must have at least 1 byte"));
        }
        Ok(Schema { version, columns })
    }

    /// Checks that `row` has one value for each column, that each fixed value has its width, and that no value is NULL in a column that is not nullable. A mismatch gives SQLSTATE `XX000`, because the layer above checks the constraints first.
    pub(crate) fn check(&self, row: &Row) -> Result<()> {
        row.header.encode()?;
        if row.values.len() != self.columns.len() {
            return Err(Error::internal(format!(
                "the row {} has {} values for {} columns",
                row.id,
                row.values.len(),
                self.columns.len()
            )));
        }
        for (i, (column, value)) in self.columns.iter().zip(&row.values).enumerate() {
            let ok = match (value, column.width) {
                (None, _) => column.nullable,
                (Some(v), Width::Fixed(w)) => v.len() == usize::from(w),
                (Some(_), Width::Variable) => true,
            };
            if !ok {
                return Err(Error::internal(format!(
                    "the value of column {i} of the row {} does not match its column",
                    row.id
                )));
            }
        }
        Ok(())
    }
}

/// The header size of a row on a leaf: the row id, the version header and `xmin`.
pub(crate) const ROW_FIXED: usize = 8 + VersionHeader::SIZE + 4;

#[cfg(test)]
mod tests {
    use rupg_common::RowId;

    use super::*;

    #[test]
    fn checks() {
        let fixed = Column { width: Width::Fixed(4), nullable: false };
        let text = Column { width: Width::Variable, nullable: true };
        let schema = Schema::new(1, vec![fixed, text]).unwrap();
        assert_eq!((fixed.minipage(10), text.minipage(10)), (40, 2 + 22));
        let row = |values| Row {
            id: RowId::from_bits(1),
            header: VersionHeader::default(),
            xmin: 0,
            xmax: 0,
            values,
        };
        schema.check(&row(vec![Some(vec![1; 4]), None])).unwrap();
        assert!(schema.check(&row(vec![None, None])).is_err());
        assert!(schema.check(&row(vec![Some(vec![1; 3]), None])).is_err());
        assert!(schema.check(&row(vec![Some(vec![1; 4])])).is_err());
        let many = vec![fixed; Schema::MAX_COLUMNS + 1];
        assert_eq!(Schema::new(1, many).unwrap_err().state(), SqlState::TOO_MANY_COLUMNS);
        assert!(Schema::new(1, vec![Column { width: Width::Fixed(0), nullable: false }]).is_err());
    }
}
