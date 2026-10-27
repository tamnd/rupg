//! The foreign-data wrappers of `pg_foreign_data_wrapper`, the foreign servers of `pg_foreign_server`, the user mappings of `pg_user_mapping` and the foreign tables of `pg_foreign_table`. They change as `CreateForeignDataWrapper`, `AlterForeignDataWrapper`, `CreateForeignServer`, `AlterForeignServer`, `CreateUserMapping`, `AlterUserMapping` and `CreateForeignTable` of `foreigncmds.c` change them, with the same rows of `pg_depend`.

use rupg_common::Result;

use super::{Catalog, DepKind, NewRelation, ObjRef, PG_CLASS, PG_PROC, RelKind, no_object};

/// The OID of `pg_foreign_data_wrapper`.
pub const PG_FOREIGN_DATA_WRAPPER: u32 = 2328;
/// The OID of `pg_foreign_server`.
pub const PG_FOREIGN_SERVER: u32 = 1417;
/// The OID of `pg_user_mapping`.
pub const PG_USER_MAPPING: u32 = 1418;
/// The OID of `pg_foreign_table`.
pub const PG_FOREIGN_TABLE: u32 = 3118;

/// A foreign-data wrapper, one row of `pg_foreign_data_wrapper`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignDataWrapper {
    /// The OID. [`Catalog::create_fdw`] sets it.
    pub oid: u32,
    /// `fdwname`: the name.
    pub name: String,
    /// `fdwowner`: the OID of the owner role.
    pub owner: u32,
    /// `fdwhandler`: the OID of the handler function, or 0.
    pub handler: u32,
    /// `fdwvalidator`: the OID of the validator function, or 0.
    pub validator: u32,
    /// `fdwconnection`: the OID of the connection function, or 0.
    pub connection: u32,
    /// `fdwoptions`: the options as `name=value`. No options give a null array.
    pub options: Vec<String>,
}

/// A foreign server, one row of `pg_foreign_server`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignServer {
    /// The OID. [`Catalog::create_server`] sets it.
    pub oid: u32,
    /// `srvname`: the name.
    pub name: String,
    /// `srvowner`: the OID of the owner role.
    pub owner: u32,
    /// `srvfdw`: the OID of the foreign-data wrapper.
    pub fdw: u32,
    /// `srvtype`: the type, or null.
    pub kind: Option<String>,
    /// `srvversion`: the version, or null.
    pub version: Option<String>,
    /// `srvoptions`: the options as `name=value`. No options give a null array.
    pub options: Vec<String>,
}

/// A user mapping, one row of `pg_user_mapping`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserMapping {
    /// The OID. [`Catalog::create_user_mapping`] sets it.
    pub oid: u32,
    /// `umuser`: the OID of the role, or 0 for `PUBLIC`.
    pub user: u32,
    /// `umserver`: the OID of the foreign server.
    pub server: u32,
    /// `umoptions`: the options as `name=value`. No options give a null array.
    pub options: Vec<String>,
}

/// The row of `pg_foreign_table` of a foreign table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignTable {
    /// `ftserver`: the OID of the foreign server.
    pub server: u32,
    /// `ftoptions`: the options as `name=value`. No options give a null array.
    pub options: Vec<String>,
}

impl ForeignDataWrapper {
    /// The functions that the wrapper depends on.
    fn procs(&self) -> impl Iterator<Item = u32> {
        [self.handler, self.validator, self.connection].into_iter().filter(|&p| p != 0)
    }
}

impl Catalog {
    /// `CreateForeignDataWrapper`: a new foreign-data wrapper, which gets the next OID. It depends on its functions. The analyzer checks the name first. The return value is the OID of the wrapper.
    pub fn create_fdw(&mut self, mut new: ForeignDataWrapper) -> u32 {
        let oid = self.new_oid();
        new.oid = oid;
        let me = ObjRef::new(PG_FOREIGN_DATA_WRAPPER, oid);
        for proc in new.procs() {
            self.depend(me, ObjRef::new(PG_PROC, proc), DepKind::Normal);
        }
        self.fdws.insert(oid, new);
        oid
    }

    /// The foreign-data wrappers, in the order of their OIDs.
    pub fn fdws(&self) -> impl Iterator<Item = &ForeignDataWrapper> {
        self.fdws.values()
    }

    /// The foreign-data wrapper with the OID.
    pub fn fdw(&self, oid: u32) -> Option<&ForeignDataWrapper> {
        self.fdws.get(&oid)
    }

    /// `GetForeignDataWrapperByName`: the foreign-data wrapper with the name.
    pub fn fdw_by_name(&self, name: &str) -> Option<&ForeignDataWrapper> {
        self.fdws.values().find(|f| f.name == name)
    }

    /// Changes the row of the foreign-data wrapper. A change of its functions makes the dependencies on functions again, as `AlterForeignDataWrapper` does.
    pub fn alter_fdw(&mut self, oid: u32, change: impl FnOnce(&mut ForeignDataWrapper)) {
        let Some(fdw) = self.fdws.get_mut(&oid) else { return };
        let old: Vec<u32> = fdw.procs().collect();
        change(fdw);
        let new: Vec<u32> = fdw.procs().collect();
        if old != new {
            let me = ObjRef::new(PG_FOREIGN_DATA_WRAPPER, oid);
            self.depends.retain(|d| !(d.object == me && d.referenced.class == PG_PROC));
            for proc in new {
                self.depend(me, ObjRef::new(PG_PROC, proc), DepKind::Normal);
            }
        }
    }

    /// `CreateForeignServer`: a new foreign server, which gets the next OID. It depends on its foreign-data wrapper. The analyzer checks the name first. The return value is the OID of the server.
    pub fn create_server(&mut self, mut new: ForeignServer) -> u32 {
        let oid = self.new_oid();
        new.oid = oid;
        let me = ObjRef::new(PG_FOREIGN_SERVER, oid);
        self.depend(me, ObjRef::new(PG_FOREIGN_DATA_WRAPPER, new.fdw), DepKind::Normal);
        self.servers.insert(oid, new);
        oid
    }

    /// The foreign servers, in the order of their OIDs.
    pub fn servers(&self) -> impl Iterator<Item = &ForeignServer> {
        self.servers.values()
    }

    /// The foreign server with the OID.
    pub fn server(&self, oid: u32) -> Option<&ForeignServer> {
        self.servers.get(&oid)
    }

    /// `GetForeignServerByName`: the foreign server with the name.
    pub fn server_by_name(&self, name: &str) -> Option<&ForeignServer> {
        self.servers.values().find(|s| s.name == name)
    }

    /// Changes the row of the foreign server.
    pub fn alter_server(&mut self, oid: u32, change: impl FnOnce(&mut ForeignServer)) {
        if let Some(server) = self.servers.get_mut(&oid) {
            change(server);
        }
    }

    /// `CreateUserMapping`: a new user mapping, which gets the next OID. It depends on its server. The analyzer checks that the role has no mapping for the server first. The return value is the OID of the mapping.
    pub fn create_user_mapping(&mut self, mut new: UserMapping) -> u32 {
        let oid = self.new_oid();
        new.oid = oid;
        let me = ObjRef::new(PG_USER_MAPPING, oid);
        self.depend(me, ObjRef::new(PG_FOREIGN_SERVER, new.server), DepKind::Normal);
        self.user_mappings.insert(oid, new);
        oid
    }

    /// The user mappings, in the order of their OIDs.
    pub fn user_mappings(&self) -> impl Iterator<Item = &UserMapping> {
        self.user_mappings.values()
    }

    /// The user mapping with the OID.
    pub fn user_mapping(&self, oid: u32) -> Option<&UserMapping> {
        self.user_mappings.get(&oid)
    }

    /// The syscache `USERMAPPINGUSERSERVER`: the mapping of the role, or of `PUBLIC` for 0, for the server.
    pub fn user_mapping_for(&self, user: u32, server: u32) -> Option<&UserMapping> {
        self.user_mappings.values().find(|m| m.user == user && m.server == server)
    }

    /// Changes the options of the user mapping.
    pub fn set_user_mapping_options(&mut self, oid: u32, options: Vec<String>) {
        if let Some(mapping) = self.user_mappings.get_mut(&oid) {
            mapping.options = options;
        }
    }

    /// `heap_create_with_catalog` for a foreign table. The relation has no storage. [`Catalog::set_foreign_table`] then gives its row of `pg_foreign_table`.
    pub fn create_foreign_table(&mut self, new: NewRelation) -> Result<u32> {
        let oid = self.create_relation(new, RelKind::Foreign)?;
        self.relation_mut(oid)?.file = 0;
        Ok(oid)
    }

    /// `CreateForeignTable`: the row of `pg_foreign_table`, and the normal dependency of the table on its server.
    pub fn set_foreign_table(
        &mut self,
        table: u32,
        server: u32,
        options: Vec<String>,
    ) -> Result<()> {
        self.relation_mut(table)?.foreign = Some(ForeignTable { server, options });
        self.depend(
            ObjRef::new(PG_CLASS, table),
            ObjRef::new(PG_FOREIGN_SERVER, server),
            DepKind::Normal,
        );
        Ok(())
    }

    /// `ATExecAlterColumnGenericOptions`: the options of the column `column`, from 0, of the foreign table.
    ///
    /// # Errors
    ///
    /// An internal error for a relation or a column that is not in the catalog.
    pub fn set_column_fdw_options(
        &mut self,
        table: u32,
        column: usize,
        options: Vec<String>,
    ) -> Result<()> {
        let rel = self.relation_mut(table)?;
        let Some(found) = rel.columns.get_mut(column) else {
            return Err(no_object(&format!("column {column} of relation"), table));
        };
        found.fdw_options = options;
        Ok(())
    }
}
