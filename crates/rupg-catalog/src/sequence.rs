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

    /// `getIdentitySequence`: the sequence of an identity column. A partition has no sequence of its own, so its column uses the sequence of the column with the same name in the topmost partitioned table.
    pub fn root_identity_sequence(&self, table: u32, column: i16) -> Option<u32> {
        let mut root = table;
        while let Some(parent) = self.relation(root).and_then(|r| r.partition_of.as_ref()) {
            root = parent.parent;
        }
        if root == table {
            return self.identity_sequence(table, column);
        }
        let name = &self.relation(table)?.column(column)?.name;
        let number = self.relation(root)?.column_number(name)?;
        self.identity_sequence(root, number)
    }

    /// The sequence that an identity column owns, which has an internal dependency on the column. The column of a partition owns no sequence.
    pub fn identity_sequence(&self, table: u32, column: i16) -> Option<u32> {
        let target = ObjRef::column(table, column);
        self.depends
            .iter()
            .find(|d| {
                d.referenced == target && d.object.class == PG_CLASS && d.kind == DepKind::Internal
            })
            .map(|d| d.object.oid)
    }

    /// `getOwnedSequences`: the sequences that a column of the table owns, by an automatic dependency of `OWNED BY` or `SERIAL` or by the internal dependency of an identity column, in the order of the columns.
    pub fn owned_sequences(&self, table: u32) -> Vec<u32> {
        let mut owned: Vec<(i32, u32)> = self
            .depends
            .iter()
            .filter(|d| {
                d.referenced.class == PG_CLASS
                    && d.referenced.oid == table
                    && d.object.class == PG_CLASS
                    && d.object.sub == 0
                    && matches!(d.kind, DepKind::Auto | DepKind::Internal)
                    && self.relation(d.object.oid).is_some_and(|r| r.sequence.is_some())
            })
            .map(|d| (d.referenced.sub, d.object.oid))
            .collect();
        owned.sort_by_key(|&(sub, _)| sub);
        owned.into_iter().map(|(_, oid)| oid).collect()
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

rupg_common::persist_struct!(SequenceData { last_value, log_cnt, is_called });
