use std::collections::BTreeMap;

use rupg_file::{PAGE_SIZE, PageHeader, PageKind};
use rupg_platform::sim::{SimClock, SimEntropy, SimIo, SimRng};
use rupg_types::TypeId;

use super::*;

const PATH: &str = "dir/app.rupg";

type Rows = BTreeMap<RowId, Vec<Datum>>;

fn platform(io: &SimIo, seed: u64) -> Platform {
    Platform {
        io: Arc::new(io.clone()),
        clock: Arc::new(SimClock::new(Hlc::EPOCH_UNIX_MS + 1_000)),
        entropy: Arc::new(SimEntropy::new(seed)),
    }
}

/// Small sizes, so that the tests use few pages of memory.
fn options() -> Options {
    Options {
        memory_limit: 16 << 20,
        shared_buffers: 4 << 20,
        log_extents: 1,
        ..Options::default()
    }
}

fn open(io: &SimIo, seed: u64, options: &Options) -> Database {
    Database::open_on(&platform(io, seed), Path::new(PATH), options).unwrap()
}

fn columns() -> Vec<ColumnDef> {
    let col = |name: &str, ty, nullable| ColumnDef { name: name.into(), ty, nullable };
    vec![
        col("id", TypeId::INT8, false),
        col("name", TypeId::TEXT, true),
        col("ok", TypeId::BOOL, true),
    ]
}

fn values(rng: &mut SimRng) -> Vec<Datum> {
    let name = match rng.below(4) {
        0 => Datum::Null,
        n => Datum::Text("x".repeat(rng.below(40 * n) as usize)),
    };
    vec![Datum::Int8(rng.below(1_000_000) as i64), name, Datum::Bool(rng.below(2) == 0)]
}

fn rows(db: &Database, table: &Table) -> Rows {
    let t = db.transaction_with(Isolation::RepeatableRead).unwrap();
    t.scan(table, ..).unwrap().into_iter().collect()
}

fn check(io: &SimIo) -> Result<crate::Report> {
    crate::check_on(&platform(io, 0), Path::new(PATH))
}

/// The slot generation, which each checkpoint increases.
fn generation(db: &Database) -> u64 {
    db.shared.store.slot().1.generation
}

#[test]
fn a_table_keeps_its_rows_after_a_close() {
    let io = SimIo::new(1);
    let db = open(&io, 1, &options());
    let t = db.create_table("people", columns()).unwrap();
    let mut tx = db.transaction().unwrap();
    let a = tx.insert(&t, &[Datum::Int8(1), Datum::Text("ann".into()), Datum::Null]).unwrap();
    let b = tx.insert(&t, &[Datum::Int8(2), Datum::Null, Datum::Bool(true)]).unwrap();
    let c = tx.insert(&t, &[Datum::Int8(3), Datum::Text("cy".into()), Datum::Bool(false)]).unwrap();
    // The transaction sees its own changes.
    assert!(
        tx.update(&t, b, &[Datum::Int8(2), Datum::Text("bo".into()), Datum::Bool(true)]).unwrap()
    );
    assert!(tx.delete(&t, c).unwrap());
    assert_eq!(tx.get(&t, c).unwrap(), None);
    tx.commit().unwrap();
    let mut tx = db.transaction().unwrap();
    assert!(tx.delete(&t, a).unwrap());
    drop(tx);

    let tx = db.transaction().unwrap();
    assert_eq!(
        tx.get(&t, a).unwrap(),
        Some(vec![Datum::Int8(1), Datum::Text("ann".into()), Datum::Null])
    );
    assert_eq!(tx.scan(&t, b..).unwrap().len(), 1);
    assert_eq!(tx.scan(&t, ..b).unwrap().len(), 1);
    assert_eq!(tx.scan(&t, (Bound::Excluded(a), Bound::Unbounded)).unwrap().len(), 1);
    assert_eq!(tx.scan(&t, a..=b).unwrap().len(), 2);
    let mut first = Vec::new();
    tx.scan_with(&t, .., |id, _| {
        first.push(id);
        Ok(false)
    })
    .unwrap();
    assert_eq!(first, vec![a]);
    drop(tx);
    let before = rows(&db, &t);
    db.clone().close().unwrap();
    assert!(db.shared.store.slot().1.clean_shutdown);
    drop((db, t));

    let db = open(&io, 1, &options());
    assert_eq!(db.table_names(), vec!["people".to_owned()]);
    let t = db.table("people").unwrap();
    assert_eq!((t.name(), t.oid(), t.columns()), ("people", Oid::FIRST_USER, &columns()[..]));
    assert_eq!(rows(&db, &t), before);
    // A new row id comes after each row id that the file had.
    let mut tx = db.transaction().unwrap();
    assert!(tx.insert(&t, &values(&mut SimRng::new(1))).unwrap() > c);
    tx.commit().unwrap();
}

#[test]
fn the_catalog_gives_errors_as_postgresql() {
    let io = SimIo::new(2);
    let db = open(&io, 2, &options());
    let t = db.create_table("t", columns()).unwrap();
    let state = |r: Result<Table>| r.unwrap_err().state();
    assert_eq!(state(db.create_table("t", columns())), SqlState::DUPLICATE_TABLE);
    assert_eq!(state(db.table("u")), SqlState::UNDEFINED_TABLE);
    let mut twice = columns();
    twice[1].name = "id".into();
    assert_eq!(state(db.create_table("u", twice)), SqlState::DUPLICATE_COLUMN);
    assert_eq!(db.table_names(), vec!["t".to_owned()]);
    let u = db.create_table("u", columns()).unwrap();
    assert_eq!(u.oid(), Oid(Oid::FIRST_USER.0 + 1));

    let mut tx = db.transaction().unwrap();
    let wrong = tx.insert(&t, &[Datum::Int4(1), Datum::Null, Datum::Null]).unwrap_err();
    assert_eq!(wrong.state(), SqlState::DATATYPE_MISMATCH);
    let null = tx.insert(&t, &[Datum::Null, Datum::Null, Datum::Null]).unwrap_err();
    assert_eq!(null.state(), SqlState::NOT_NULL_VIOLATION);
    // A table of another database is not a table of this one.
    let other = open(&SimIo::new(3), 3, &options());
    let theirs = other.create_table("t", columns()).unwrap();
    let err = tx.insert(&theirs, &values(&mut SimRng::new(2))).unwrap_err();
    assert_eq!(err.state(), SqlState::UNDEFINED_TABLE);
    assert!(tx.get(&theirs, RowId::from_bits(1)).is_err());

    let missing = Options { create: false, ..options() };
    let err = Database::open_on(&platform(&io, 2), Path::new("other.rupg"), &missing).unwrap_err();
    assert_eq!(err.state(), SqlState::UNDEFINED_FILE);
}

/// A file that a crash left with zero bytes opens as a new file.
#[test]
fn an_empty_file_opens_as_a_new_file() {
    let io = SimIo::new(4);
    io.open(Path::new(PATH), OpenMode::CreateNew).unwrap();
    let db = open(&io, 4, &options());
    assert!(db.table_names().is_empty());
    db.create_table("t", columns()).unwrap();
    drop(db);
    io.crash_random().unwrap();
    assert_eq!(open(&io, 4, &options()).table_names(), vec!["t".to_owned()]);
}

/// Random transactions on two tables, with tables that come and checkpoints that the log size starts, and a power cut at a random point. After each cut the file holds each acknowledged commit and nothing of the transactions that did not commit.
#[test]
fn a_crash_keeps_each_acknowledged_commit() {
    let options = Options { checkpoint_log: Some(64 << 10), ..options() };
    let mut checkpoints = 0;
    for seed in 0..6u64 {
        let io = SimIo::new(seed);
        let mut rng = SimRng::new(seed + 100);
        let mut models: BTreeMap<String, Rows> = BTreeMap::new();
        for round in 0..5 {
            let db = open(&io, seed, &options);
            let start = generation(&db);
            for (name, model) in &models {
                assert_eq!(
                    &rows(&db, &db.table(name).unwrap()),
                    model,
                    "seed {seed} round {round}"
                );
            }
            if models.len() < 3 {
                let name = format!("t{round}");
                db.create_table(&name, columns()).unwrap();
                models.insert(name, Rows::new());
            }
            for _ in 0..rng.below(80) {
                let name =
                    models.keys().nth(rng.below(models.len() as u64) as usize).unwrap().clone();
                let table = db.table(&name).unwrap();
                let model = models.get_mut(&name).unwrap();
                let mut next = model.clone();
                let mut tx = db.transaction().unwrap();
                for _ in 0..1 + rng.below(8) {
                    let some = (!next.is_empty())
                        .then(|| *next.keys().nth(rng.below(next.len() as u64) as usize).unwrap());
                    match (rng.below(10), some) {
                        (0..5, _) | (_, None) => {
                            let v = values(&mut rng);
                            next.insert(tx.insert(&table, &v).unwrap(), v);
                        }
                        (5..8, Some(id)) => {
                            let v = values(&mut rng);
                            assert!(tx.update(&table, id, &v).unwrap());
                            next.insert(id, v);
                        }
                        (_, Some(id)) => {
                            assert!(tx.delete(&table, id).unwrap());
                            next.remove(&id);
                        }
                    }
                }
                if rng.below(8) == 0 {
                    tx.rollback().unwrap();
                } else {
                    tx.commit().unwrap();
                    *model = next;
                }
            }
            // A transaction that is open at the power cut.
            let name = models.keys().next().unwrap().clone();
            let mut open = db.transaction().unwrap();
            let table = db.table(&name).unwrap();
            for _ in 0..rng.below(20) {
                open.insert(&table, &values(&mut rng)).unwrap();
            }
            if rng.below(3) == 0 {
                db.checkpoint().unwrap();
            }
            checkpoints += generation(&db) - start;
            std::mem::forget(open);
            drop((db, table));
            io.crash_random().unwrap();
            let report = check(&io).unwrap_or_else(|e| panic!("seed {seed} round {round}: {e}"));
            assert_eq!(report.tables as usize, models.len());
        }
    }
    assert!(checkpoints > 30, "{checkpoints}");
}

#[test]
fn check_finds_a_lost_page_and_a_damaged_leaf() {
    let io = SimIo::new(5);
    let db = open(&io, 5, &options());
    let t = db.create_table("t", columns()).unwrap();
    let mut rng = SimRng::new(5);
    for _ in 0..20 {
        let mut tx = db.transaction().unwrap();
        for _ in 0..50 {
            tx.insert(&t, &values(&mut rng)).unwrap();
        }
        tx.commit().unwrap();
    }
    db.checkpoint().unwrap();
    let mut tx = db.transaction().unwrap();
    tx.insert(&t, &values(&mut rng)).unwrap();
    tx.commit().unwrap();
    drop((db, t));
    // The check reads the pages of the checkpoint, and the last commit is only in the log.
    let report = check(&io).unwrap();
    assert_eq!((report.tables, report.rows, report.log_blocks), (1, 1_000, 1));
    assert!(report.pages > 3, "{report:?}");

    // A tree that the catalog does not name.
    let db = open(&io, 5, &options());
    let def = TableDef::new(Oid(Oid::FIRST_USER.0 + 9), "lost", columns()).unwrap();
    rupg_table::Table::create(&**db.shared.txns.pages(), def, ShardId(0)).unwrap();
    db.checkpoint().unwrap();
    drop(db);
    let err = check(&io).unwrap_err();
    assert_eq!(err.state(), SqlState::DATA_CORRUPTED, "{err}");
    assert!(err.to_string().contains("does not reach"), "{err}");

    // One bit of each copy of a leaf in the file.
    let io = SimIo::new(6);
    let db = open(&io, 6, &options());
    let t = db.create_table("t", columns()).unwrap();
    let mut tx = db.transaction().unwrap();
    tx.insert(&t, &values(&mut rng)).unwrap();
    tx.commit().unwrap();
    db.close().unwrap();
    drop(t);
    check(&io).unwrap();
    let file = io.open(Path::new(PATH), OpenMode::ReadWrite).unwrap();
    let mut page = [0u8; PAGE_SIZE];
    let mut leaves = 0;
    for n in 0..file.size().unwrap() / PAGE_SIZE as u64 {
        file.read_at(n * PAGE_SIZE as u64, &mut page).unwrap();
        if PageHeader::read(&page).is_ok_and(|h| h.kind == PageKind::HotLeaf) {
            page[PAGE_SIZE - 1] ^= 1;
            file.write_at(n * PAGE_SIZE as u64, &page).unwrap();
            leaves += 1;
        }
    }
    assert!(leaves > 0);
    assert_eq!(check(&io).unwrap_err().state(), SqlState::DATA_CORRUPTED);
}
