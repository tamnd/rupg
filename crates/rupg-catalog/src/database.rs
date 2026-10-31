//! The rows of `pg_database`. The catalog of a database holds the objects of that database, and the rows of `pg_database` are the same for all databases of the cluster, so the store of the server keeps them.

/// The OID of `template1`.
pub const TEMPLATE1: u32 = 1;
/// The OID of `template0`.
pub const TEMPLATE0: u32 = 4;
/// The OID of `postgres`.
pub const POSTGRES: u32 = 5;
/// `DATCONNLIMIT_UNLIMITED`.
pub const UNLIMITED: i32 = -1;
/// `datfrozenxid` of the databases of a new cluster: the `VACUUM FREEZE` of `initdb` of PostgreSQL 19 runs in the transaction 654. It is also `oldestXid` and `oldestClogXid` of the cluster.
pub const INITDB_FROZEN_XID: u32 = 654;

/// A row of `pg_database`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Database {
    pub oid: u32,
    pub name: String,
    /// `datdba`, the owner.
    pub owner: u32,
    /// The number of the encoding, 6 for `UTF8`.
    pub encoding: i32,
    /// `datlocprovider`: `b` for builtin, `c` for libc and `i` for ICU.
    pub locale_provider: u8,
    pub is_template: bool,
    pub allow_connections: bool,
    /// `dathasloginevt`: true when the database can have event triggers of `login`.
    pub has_login_event: bool,
    /// `datconnlimit`, or [`UNLIMITED`].
    pub connection_limit: i32,
    pub frozen_xid: u32,
    pub min_mxid: u32,
    /// `dattablespace`, the default tablespace.
    pub tablespace: u32,
    pub collate: String,
    pub ctype: String,
    /// `datlocale`, the locale of the builtin or the ICU provider.
    pub locale: Option<String>,
    pub icu_rules: Option<String>,
    pub collation_version: Option<String>,
    /// `datacl` as the texts of its items, or `None` for the default privileges.
    pub acl: Option<Vec<String>>,
}

impl Database {
    /// The three databases of `initdb`, in the order of their OIDs. They are the same as the static rows of `pg_database`.
    pub fn initdb() -> [Database; 3] {
        let acl = Some(vec!["=c/postgres".to_owned(), "postgres=CTc/postgres".to_owned()]);
        let row = |oid: u32, name: &str, is_template: bool, allow_connections: bool| Database {
            oid,
            name: name.to_owned(),
            owner: 10,
            encoding: 6,
            locale_provider: b'c',
            is_template,
            allow_connections,
            has_login_event: false,
            connection_limit: UNLIMITED,
            frozen_xid: INITDB_FROZEN_XID,
            min_mxid: 1,
            tablespace: 1663,
            collate: "C".to_owned(),
            ctype: "C".to_owned(),
            locale: None,
            icu_rules: None,
            collation_version: None,
            acl: if is_template { acl.clone() } else { None },
        };
        [
            row(TEMPLATE1, "template1", true, true),
            row(TEMPLATE0, "template0", true, false),
            row(POSTGRES, "postgres", false, true),
        ]
    }
}
