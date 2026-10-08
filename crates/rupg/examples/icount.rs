//! One case of the instruction count set of spec/21 section 21.14. `cargo xtask bench` runs this program under `perf stat` and does not need it to print.
//!
//! Usage: `icount list` gives the names of the cases, and `icount <case> <setup|run> <file>` runs one case. The file must not exist. `setup` opens the database, makes the table and loads the rows that the case reads, and closes the database. `run` does the same and then the work of the case before the close. The count of the case is the count of `run` less the count of `setup`.

use rupg::{ColumnDef, Database, Datum, Options, Result, RowId, Table, TypeId};

/// The rows that a case loads, inserts or reads.
const ROWS: i32 = 10_000;
/// The transactions of the case `commit`.
const COMMITS: i32 = 100;

/// The cases, with true when the case reads the rows that the setup loads.
const CASES: [(&str, bool); 6] = [
    ("insert", false),
    ("get", true),
    ("scan", true),
    ("update", true),
    ("delete", true),
    ("commit", false),
];

fn row(i: i32) -> [Datum; 2] {
    [Datum::Int4(i), Datum::Text(format!("row {i}"))]
}

fn load(db: &Database, t: &Table) -> Result<Vec<RowId>> {
    let mut tx = db.transaction()?;
    let ids = (0..ROWS).map(|i| tx.insert(t, &row(i))).collect::<Result<Vec<_>>>()?;
    tx.commit()?;
    Ok(ids)
}

fn run(case: &str, db: &Database, t: &Table, ids: &[RowId]) -> Result<()> {
    match case {
        "insert" => {
            load(db, t)?;
        }
        "get" => {
            let tx = db.transaction()?;
            for id in ids {
                tx.get(t, *id)?;
            }
            tx.commit()?;
        }
        "scan" => {
            let tx = db.transaction()?;
            let mut count = 0;
            tx.scan_with(t, .., |_, _| {
                count += 1;
                Ok(true)
            })?;
            assert_eq!(count, ids.len(), "the scan must see each loaded row");
            tx.commit()?;
        }
        "update" => {
            let mut tx = db.transaction()?;
            for (i, id) in (0..).zip(ids) {
                tx.update(t, *id, &row(i + ROWS))?;
            }
            tx.commit()?;
        }
        "delete" => {
            let mut tx = db.transaction()?;
            for id in ids {
                tx.delete(t, *id)?;
            }
            tx.commit()?;
        }
        "commit" => {
            for i in 0..COMMITS {
                let mut tx = db.transaction()?;
                tx.insert(t, &row(i))?;
                tx.commit()?;
            }
        }
        _ => unreachable!("the case is checked in main"),
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() == 1 && args[0] == "list" {
        for (name, _) in CASES {
            println!("{name}");
        }
        return Ok(());
    }
    let [case, mode, file] = args.as_slice() else {
        eprintln!("usage: icount list | icount <case> <setup|run> <file>");
        std::process::exit(2);
    };
    let Some(&(_, loads)) = CASES.iter().find(|(name, _)| name == case) else {
        eprintln!("unknown case {case}");
        std::process::exit(2);
    };
    let options = Options { window_pages: Some(1 << 16), ..Options::default() };
    let db = Database::open(file, options)?;
    let t = db.create_table(
        "t",
        vec![
            ColumnDef { name: "id".into(), ty: TypeId::INT4, nullable: false },
            ColumnDef { name: "name".into(), ty: TypeId::TEXT, nullable: true },
        ],
    )?;
    let ids = if loads { load(&db, &t)? } else { Vec::new() };
    match mode.as_str() {
        "setup" => {}
        "run" => run(case, &db, &t, &ids)?,
        _ => {
            eprintln!("the mode must be setup or run");
            std::process::exit(2);
        }
    }
    db.close()
}
