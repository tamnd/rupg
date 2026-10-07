//! `rupg check`: a check of a database file that does not change the file (spec/21 section 21.9.2).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;

use rupg_buffer::{BufferPool, FileStore, PoolConfig, PoolMode};
use rupg_common::{Error, Hlc, Result, RowId, ShardId, SqlState};
use rupg_file::PageId;
use rupg_log::{RecordReader, Recovery};
use rupg_platform::{MemoryPool, OpenMode};

use crate::Platform;
use crate::catalog::{Contents, Entry};

/// The pool of the check: 64 frames. The check reads each page once, so a small pool is enough. The frames of the table form are zeroed when the pool is made, so a large pool makes each check slow.
const POOL: u64 = 1 << 20;

/// The counts of a check that found no error.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The logical pages in use. Each one is the catalog page or a page of a table.
    pub pages: u64,
    /// The tables.
    pub tables: u64,
    /// The rows of the tables.
    pub rows: u64,
    /// The blocks of the log after the redo position that recovery would replay.
    pub log_blocks: u64,
    /// The bytes of those blocks, with their headers and without the fill after a block. This is the log that an open replays.
    pub log_bytes: u64,
}

/// Checks the database file at `path` on the operating system. See [`check_on`].
pub fn check(path: impl AsRef<Path>) -> Result<Report> {
    check_on(&Platform::os(), path.as_ref())
}

/// Checks the database file at `path` on `platform`. The check opens the file to read and does not replay the log, so it checks the pages of the last checkpoint. It does these steps:
///
/// 1. It reads the header slots, the page table, the free space map and the pending free lists, and checks that each physical page has one use.
/// 2. It reads the catalog page and checks its checksum.
/// 3. For each table, it reads each page of the tree, checks the shape of the tree and decodes each row against the columns of the table. A row in the file must have no owner, a commit timestamp at or below the checkpoint, and a row id below the next row id of the table.
/// 4. It checks that each logical page in use is the catalog page or a page of exactly one table.
/// 5. It reads each block of the log after the redo position, checks its checksum and decodes its records. Each record must name a table of the catalog.
///
/// Damage gives SQLSTATE `XX001`, and a wrong count in the free space map gives `XX000`. A missing file gives `58P01`.
pub fn check_on(platform: &Platform, path: &Path) -> Result<Report> {
    if !platform.io.exists(path)? {
        return Err(Error::new(
            SqlState::UNDEFINED_FILE,
            format!("database file \"{}\" does not exist", path.display()),
        ));
    }
    let store = Arc::new(FileStore::open(platform.io.open(path, OpenMode::Read)?)?);
    store.check()?;
    let (_, slot) = store.slot();
    let contents = match slot.catalog.page {
        0 => Contents::new(),
        _ => Contents::read(&store, slot.catalog)?,
    };
    let mut reached = BTreeSet::new();
    if slot.catalog.page != 0 {
        reached.insert(PageId(slot.catalog.page));
    }
    let mut report = Report { tables: contents.tables.len() as u64, ..Report::default() };
    let mut names = BTreeSet::new();
    let mut oids = BTreeMap::new();
    let memory = MemoryPool::new(2 * POOL);
    // The check reads each page once, and the table form is fast to make and to free.
    let config = PoolConfig { mode: Some(PoolMode::Table), ..PoolConfig::new(POOL) };
    let pool = BufferPool::new(store.clone(), &memory, config)?;
    for e in &contents.tables {
        let (name, oid) = (e.def.name(), e.def.oid());
        if !names.insert(name) || oids.insert(oid, e).is_some() {
            return Err(Error::corrupted(format!(
                "the catalog has two tables with the name \"{name}\" or the OID {oid}"
            )));
        }
        report.rows += check_table(&pool, e, slot.checkpoint, &mut reached)?;
    }

    let allocated = store.allocated();
    if let Some(page) = allocated.iter().find(|p| !reached.contains(p)) {
        return Err(Error::corrupted(format!(
            "logical page {page} is in use, but the catalog does not reach it"
        )));
    }
    report.pages = allocated.len() as u64;

    let rec = Recovery::scan(&store)?;
    for found in rec.safe() {
        let block = rec.read(&store, found)?;
        for record in RecordReader::new(&block.body) {
            let record = record?;
            if !oids.contains_key(&record.table) {
                return Err(Error::corrupted(format!(
                    "the log block of ring {} at position {} names the table {}, which the catalog does not have",
                    block.ring, block.position, record.table
                )));
            }
        }
        report.log_blocks += 1;
        report.log_bytes += found.placed.end - found.placed.position;
    }
    Ok(report)
}

/// Checks the tree and the rows of one table, adds its pages to `reached`, and gives the number of rows.
fn check_table(
    pool: &BufferPool,
    e: &Entry,
    checkpoint: Hlc,
    reached: &mut BTreeSet<PageId>,
) -> Result<u64> {
    let def = &e.def;
    let table = rupg_table::Table::open(def.clone(), ShardId(0), e.root, e.next_local);
    let mut rows = 0;
    let mut last: Option<RowId> = None;
    let bad = |id: RowId, what: &str| {
        Error::corrupted(format!("the row {id} of table \"{}\" {what}", def.name()))
    };
    table.hot().check_with(
        pool,
        &mut |page| {
            if !reached.insert(page) {
                return Err(Error::corrupted(format!(
                    "logical page {page} is in two places, one is table \"{}\"",
                    def.name()
                )));
            }
            Ok(())
        },
        &mut |row| {
            let id = row.id;
            if last.is_some_and(|l| l >= id) {
                return Err(bad(id, "is not in row id order"));
            }
            last = Some(id);
            if row.header.owner().is_some() {
                return Err(bad(id, "has an owner"));
            }
            if Hlc::from_bits(row.header.stamp) > checkpoint {
                return Err(bad(id, "has a commit timestamp after the checkpoint"));
            }
            if id.shard() != ShardId(0) || id.local() >= e.next_local {
                return Err(bad(id, "has a row id that the table did not give"));
            }
            def.decode(&row.values).map_err(|err| bad(id, &format!("does not decode: {err}")))?;
            rows += 1;
            Ok(())
        },
    )?;
    Ok(rows)
}
