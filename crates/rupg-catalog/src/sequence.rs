//! The sequences: the row of the file of a sequence, and the changes of `AlterSequence` and `process_owned_by`.

use rupg_common::{Error, Result};

use super::{Catalog, DepKind, ObjRef, PG_CLASS, SequenceInfo};

/// The row of the file of a sequence, `FormData_pg_sequence_data`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SequenceData {
    /// The last value that a call fetched, or the next value when `is_called` is false.
    pub last_value: i64,
    /// The number of values that the session can fetch before it writes the next WAL record.
    pub log_cnt: i64,
    /// False when the next call of `nextval` gives `last_value` itself.
    pub is_called: bool,
}

impl SequenceData {
    /// The row of a new file: `last_value` is the value that the next call gives.
    pub fn new(last_value: i64) -> SequenceData {
        SequenceData { last_value, log_cnt: 0, is_called: false }
    }
}

impl Catalog {
    /// The update of `pg_sequence` in `AlterSequence`. With `rewrite`, the sequence gets a new file with this row, as `RelationSetNewRelfilenumber` and `fill_seq_with_data` make it. The new file takes an OID, as `GetNewRelFileNumber` does.
    pub fn alter_sequence(
        &mut self,
        oid: u32,
        mut info: SequenceInfo,
        rewrite: Option<SequenceData>,
    ) -> Result<()> {
        let file = match rewrite {
            Some(data) => {
                info.data = data;
                self.new_oid()
            }
            None => info.file,
        };
        info.file = file;
        let rel = self.relation_mut(oid)?;
        let Some(old) = &mut rel.sequence else {
            return Err(Error::internal(format!("cache lookup failed for sequence {oid}")));
        };
        *old = info;
        Ok(())
    }

    /// `process_owned_by`: the sequence loses its dependencies of `kind` on relations, and then gets one on the column when `column` is given.
    pub fn set_owned_by(&mut self, sequence: u32, column: Option<(u32, i16)>, kind: DepKind) {
        let me = ObjRef::new(PG_CLASS, sequence);
        self.depends
            .retain(|d| !(d.object == me && d.referenced.class == PG_CLASS && d.kind == kind));
        if let Some((table, column)) = column {
            self.depend(me, ObjRef::column(table, column), kind);
        }
    }
}
