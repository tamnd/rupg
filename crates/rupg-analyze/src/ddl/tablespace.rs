//! The tablespace of a new relation, as `DefineRelation` and `DefineIndex` of `tablecmds.c` and `indexcmds.c` choose it, with `GetDefaultTablespace` of `tablespace.c`.

use rupg_common::{Error, Result, SqlState};

use super::Definer;

/// `GLOBALTABLESPACE_OID`: the tablespace of the shared catalogs.
const GLOBAL_TABLESPACE: u32 = 1664;

impl Definer<'_, '_> {
    /// The tablespace of a new relation. `name` is the name of the `TABLESPACE` clause. `inherited` is the tablespace that the relation takes when it has no clause, as a partition takes the tablespace of its parent. A temporary relation has `temp`, and a partitioned table or a partitioned index has `partitioned`. The result is 0 for the default tablespace of the database, as `heap_create` stores it.
    pub(super) fn relation_tablespace(
        &self,
        name: Option<&str>,
        inherited: u32,
        temp: bool,
        partitioned: bool,
    ) -> Result<u32> {
        let database = self.an.env.database_tablespace();
        let mut tablespace = match name {
            Some(name) => {
                let oid = self.an.env.tablespace_oid(name).ok_or_else(|| {
                    Error::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!("tablespace \"{name}\" does not exist"),
                    )
                })?;
                if partitioned && oid == database {
                    return Err(partitioned_default());
                }
                oid
            }
            None => inherited,
        };
        if tablespace == 0 {
            tablespace = self.default_tablespace(temp, partitioned)?;
        }
        if tablespace != 0
            && tablespace != database
            && !self.an.env.tablespace_create_allowed(tablespace)
        {
            let name = self.an.env.tablespace_name(tablespace).unwrap_or_default();
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for tablespace {name}"),
            ));
        }
        if tablespace == GLOBAL_TABLESPACE {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                "only shared relations can be placed in pg_global tablespace",
            ));
        }
        Ok(if tablespace == database { 0 } else { tablespace })
    }

    /// `GetDefaultTablespace`: the tablespace of `default_tablespace`, or 0 when the setting is empty, when the tablespace is not there, or for a temporary relation. The default tablespace of the database gives 0, but a partitioned relation cannot use it.
    fn default_tablespace(&self, temp: bool, partitioned: bool) -> Result<u32> {
        if temp {
            return Ok(0);
        }
        let name = self.an.env.default_tablespace();
        if name.is_empty() {
            return Ok(0);
        }
        let Some(oid) = self.an.env.tablespace_oid(&name) else { return Ok(0) };
        if oid == self.an.env.database_tablespace() {
            if partitioned {
                return Err(partitioned_default());
            }
            return Ok(0);
        }
        Ok(oid)
    }
}

fn partitioned_default() -> Error {
    Error::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        "cannot specify default tablespace for partitioned relations",
    )
}
