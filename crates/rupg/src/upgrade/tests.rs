use std::collections::BTreeMap;

use rupg_common::Hlc;
use rupg_platform::sim::{SimClock, SimEntropy, SimIo};
use rupg_table::ColumnDef;
use rupg_txn::Isolation;
use rupg_types::{Datum, TypeId};

use super::*;

const OLD: &str = "dir/old.rupg";
const NEW: &str = "dir/new.rupg";

type Rows = BTreeMap<RowId, Vec<Datum>>;

fn platform(io: &SimIo) -> Platform {
    Platform {
        io: Arc::new(io.clone()),
        clock: Arc::new(SimClock::new(Hlc::EPOCH_UNIX_MS + 1_000)),
        entropy: Arc::new(SimEntropy::new(7)),
    }
}

/// Small sizes, so that the tests use few pages of memory, and a small window, so that each open is fast.
fn options() -> Options {
    Options {
        memory_limit: 16 << 20,
        shared_buffers: 4 << 20,
        log_extents: 1,
        window_pages: Some(1 << 16),
        ..Options::default()
    }
}

fn open(io: &SimIo, path: &str) -> Database {
    Database::open_on(&platform(io), Path::new(path), &options()).unwrap()
}

fn upgrade(io: &SimIo) -> Result<Upgrade> {
    upgrade_on(&platform(io), Path::new(OLD), Path::new(NEW), &options())
}

fn columns() -> Vec<ColumnDef> {
    vec![
        ColumnDef { name: "id".into(), ty: TypeId::INT8, nullable: false },
        ColumnDef { name: "name".into(), ty: TypeId::TEXT, nullable: true },
    ]
}

fn row(n: i64) -> Vec<Datum> {
    let name = if n % 3 == 0 { Datum::Null } else { Datum::Text(format!("row {n}")) };
    vec![Datum::Int8(n), name]
}

fn rows(db: &Database, name: &str) -> Rows {
    let table = db.table(name).unwrap();
    let t = db.transaction_with(Isolation::RepeatableRead).unwrap();
    t.scan(&table, ..).unwrap().into_iter().collect()
}

/// Two tables with rows, and some rows deleted, so that the row ids have gaps.
fn fill(db: &Database) -> (Rows, Rows) {
    let a = db.create_table("a", columns()).unwrap();
    let b = db.create_table("b", vec![columns()[0].clone()]).unwrap();
    let mut t = db.transaction().unwrap();
    let ids: Vec<RowId> = (0..300).map(|n| t.insert(&a, &row(n)).unwrap()).collect();
    for n in 0..40 {
        t.insert(&b, &[Datum::Int8(n)]).unwrap();
    }
    t.commit().unwrap();
    let mut t = db.transaction().unwrap();
    for id in ids.iter().step_by(7) {
        assert!(t.delete(&a, *id).unwrap());
    }
    t.commit().unwrap();
    (rows(db, "a"), rows(db, "b"))
}

#[test]
fn the_new_file_has_the_tables_and_the_rows_of_the_old_file() {
    let io = SimIo::new(1);
    let db = open(&io, OLD);
    let (a, b) = fill(&db);
    let oids = (db.table("a").unwrap().oid(), db.table("b").unwrap().oid());
    let next = db.table("a").unwrap().inner.next_local();
    db.close().unwrap();
    let before = io.contents(Path::new(OLD)).unwrap();

    let done = upgrade(&io).unwrap();
    assert_eq!(done, Upgrade { tables: 2, rows: (a.len() + b.len()) as u64 });
    assert_eq!(io.contents(Path::new(OLD)).unwrap(), before, "the old file changed");
    assert!(!io.exists(Path::new("dir/new.rupg.partial")).unwrap());
    crate::check_on(&platform(&io), Path::new(NEW)).unwrap();

    let new = open(&io, NEW);
    assert_eq!(new.table_names(), vec!["a".to_owned(), "b".to_owned()]);
    assert_eq!((new.table("a").unwrap().oid(), new.table("b").unwrap().oid()), oids);
    assert_eq!(new.table("a").unwrap().columns(), columns().as_slice());
    assert_eq!(rows(&new, "a"), a);
    assert_eq!(rows(&new, "b"), b);
    // A new row does not get the id of a row of the old file, also of a deleted row.
    let table = new.table("a").unwrap();
    let mut t = new.transaction().unwrap();
    let id = t.insert(&table, &row(1_000)).unwrap();
    t.commit().unwrap();
    assert!(id.local() >= next, "the new row has id {id}, below {next}");
    new.close().unwrap();
}

#[test]
fn the_upgrade_replays_the_log_of_the_old_file_in_memory() {
    let io = SimIo::new(2);
    let db = open(&io, OLD);
    let a = db.create_table("a", columns()).unwrap();
    db.checkpoint().unwrap();
    let mut t = db.transaction().unwrap();
    for n in 0..50 {
        t.insert(&a, &row(n)).unwrap();
    }
    t.commit().unwrap();
    // A transaction that does not commit leaves no row in the new file.
    let mut open_tx = db.transaction().unwrap();
    open_tx.insert(&a, &row(99)).unwrap();
    let committed = {
        let t = db.transaction_with(Isolation::RepeatableRead).unwrap();
        t.scan(&a, ..).unwrap().into_iter().collect::<Rows>()
    };
    drop(open_tx);
    // No clean close: the committed rows are only in the log.
    drop(db);
    let before = io.contents(Path::new(OLD)).unwrap();

    let done = upgrade(&io).unwrap();
    assert_eq!(done.rows, 50);
    assert_eq!(io.contents(Path::new(OLD)).unwrap(), before, "the old file changed");
    let new = open(&io, NEW);
    assert_eq!(rows(&new, "a"), committed);
}

#[test]
fn a_large_table_goes_into_the_new_file_in_batches() {
    let io = SimIo::new(3);
    let db = open(&io, OLD);
    let a = db.create_table("a", vec![columns()[0].clone()]).unwrap();
    let mut t = db.transaction().unwrap();
    for n in 0..(BATCH as i64 * 2 + 17) {
        t.insert(&a, &[Datum::Int8(n)]).unwrap();
    }
    t.commit().unwrap();
    let a = rows(&db, "a");
    db.close().unwrap();

    assert_eq!(upgrade(&io).unwrap().rows, BATCH * 2 + 17);
    assert_eq!(rows(&open(&io, NEW), "a"), a);
}

#[test]
fn the_upgrade_gives_errors_and_leaves_no_new_file() {
    let io = SimIo::new(4);
    let e = upgrade(&io).unwrap_err();
    assert_eq!(e.state(), SqlState::UNDEFINED_FILE, "{e}");
    assert!(!io.exists(Path::new(NEW)).unwrap());

    let db = open(&io, OLD);
    fill(&db);
    db.close().unwrap();
    // A file from a copy that stopped is replaced.
    let partial = io.open(Path::new("dir/new.rupg.partial"), OpenMode::CreateNew).unwrap();
    partial.write_at(0, b"partial").unwrap();
    drop(partial);
    upgrade(&io).unwrap();
    let e = upgrade(&io).unwrap_err();
    assert_eq!(e.state(), SqlState::DUPLICATE_FILE, "{e}");
}

#[test]
fn the_memory_layer_reads_its_writes_and_the_file_below() {
    let io = SimIo::new(5);
    let file = io.open(Path::new("f"), OpenMode::CreateNew).unwrap();
    file.write_at(0, &[1; 10_000]).unwrap();
    drop(file);
    let ro = ReadOnly { inner: Arc::new(io.clone()) };
    let f = ro.open(Path::new("f"), OpenMode::ReadWrite).unwrap();
    f.write_at(4090, &[2; 20]).unwrap();
    let mut buf = [0; 30];
    f.read_at(4080, &mut buf).unwrap();
    assert_eq!(&buf[..10], &[1; 10]);
    assert_eq!(&buf[10..30], &[2; 20]);
    f.write_at(12_000, &[3; 4]).unwrap();
    assert_eq!(f.size().unwrap(), 12_004);
    let mut buf = [9; 4];
    f.read_at(10_500, &mut buf).unwrap();
    assert_eq!(buf, [0; 4], "the bytes between the end of the file and a write are zeros");
    f.set_size(5_000).unwrap();
    f.set_size(9_000).unwrap();
    let mut buf = [9; 4];
    f.read_at(6_000, &mut buf).unwrap();
    assert_eq!(buf, [0; 4], "a cut file gives zeros after it grows again");
    assert!(f.read_at(8_998, &mut [0; 4]).is_err());
    assert_eq!(io.contents(Path::new("f")).unwrap(), vec![1; 10_000], "the file changed");
    assert!(ro.open(Path::new("g"), OpenMode::CreateNew).is_err());
}
