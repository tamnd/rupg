//! The foreign-data wrappers of `pg_foreign_data_wrapper` and the foreign servers of `pg_foreign_server`. They change as `CreateForeignDataWrapper`, `AlterForeignDataWrapper`, `CreateForeignServer` and `AlterForeignServer` of `foreigncmds.c` change them, with the same rows of `pg_depend`.

use super::{Catalog, DepKind, ObjRef, PG_PROC};

/// The OID of `pg_foreign_data_wrapper`.
pub const PG_FOREIGN_DATA_WRAPPER: u32 = 2328;
/// The OID of `pg_foreign_server`.
pub const PG_FOREIGN_SERVER: u32 = 1417;

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
}
