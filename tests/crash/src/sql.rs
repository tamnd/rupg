//! The crash test of spec/21 section 21.9 for the SQL store and the access methods of M6: a table with an index of each access method, the DML that changes it, the DDL that makes and drops the indexes, checkpoints and restarts.
//!
//! A seeded workload of SQL runs on the store of the sessions, whose image and log are on the simulated disk. Before each sync of a file, the test builds crash files from the writes that are not synced, as [`crate::run`] does. For each crash file it loads the store again, which reads the image and redoes the log, and checks three things. The rows of the table must be exactly the acknowledged commits, or those and the change that was in progress at the cut. The indexes must be the ones that the acknowledged DDL left, or those of the DDL in progress. And a query that each index answers must give the rows that the same query gives on a scan of the table, so that an index does not lose a row, keep a row that went, or keep an old version of a row.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rupg_common::Hlc;
use rupg_platform::Io;
use rupg_platform::sim::{SimClock, SimIo, SimRng};
use rupg_session::connection::{Client, Connection, Next, Start, base_settings, session_settings};
use rupg_session::store::Store;
use rupg_wire::{CANCEL_KEY_LEN, CancelKey, OutBuf};

use crate::{Config, Counts, Hooked, lock, plans};

/// The image of the store on the simulated disk. The log of each epoch is next to it.
const IMAGE: &str = "/data/crash.rupg";

/// The version 3.0 of the protocol.
const PROTOCOL_3_0: u32 = 3 << 16;

/// The indexes that the workload makes and drops, each with its access method, the statement that makes it, and the condition of a query that it answers. The primary key is an index too, and it stays.
pub const INDEXES: [(&str, &str, &str, &str); 7] = [
    ("t_pkey", "btree", "", "k BETWEEN 20 AND 60"),
    ("t_a", "btree", "CREATE INDEX t_a ON t USING btree (a, k)", "a BETWEEN 100 AND 400"),
    ("t_s", "hash", "CREATE INDEX t_s ON t USING hash (s)", "s = 'v3'"),
    ("t_arr", "gin", "CREATE INDEX t_arr ON t USING gin (arr)", "arr @> ARRAY[3]"),
    ("t_p", "gist", "CREATE INDEX t_p ON t USING gist (p)", "p <@ box '(0,0),(40,40)'"),
    ("t_r", "spgist", "CREATE INDEX t_r ON t USING spgist (r)", "r && int4range(10, 20)"),
    ("t_b", "brin", "CREATE INDEX t_b ON t USING brin (b) WITH (pages_per_range = 1)", "b < 50"),
];

/// One row of the table, after its key.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    a: i32,
    s: Option<String>,
    p: (i32, i32),
    arr: Vec<i32>,
    r: (i32, i32),
    b: i32,
}

impl Row {
    fn random(rng: &mut SimRng) -> Row {
        let small = |rng: &mut SimRng, n: u64| i32::try_from(rng.below(n)).unwrap_or(0);
        let lo = small(rng, 60);
        Row {
            a: small(rng, 1000),
            s: (rng.below(6) != 0).then(|| format!("v{}", rng.below(8))),
            p: (small(rng, 100), small(rng, 100)),
            arr: (0..rng.below(5)).map(|_| small(rng, 8)).collect(),
            r: (lo, lo + 1 + small(rng, 30)),
            b: small(rng, 200),
        }
    }

    /// The text of the columns of the row in the order of the table, as the store gives them.
    fn texts(&self, k: i32) -> Vec<Option<String>> {
        let arr: Vec<String> = self.arr.iter().map(i32::to_string).collect();
        vec![
            Some(k.to_string()),
            Some(self.a.to_string()),
            self.s.clone(),
            Some(format!("({},{})", self.p.0, self.p.1)),
            Some(format!("{{{}}}", arr.join(","))),
            Some(format!("[{},{})", self.r.0, self.r.1)),
            Some(self.b.to_string()),
        ]
    }

    /// The values of the row in SQL.
    fn sql(&self) -> String {
        let s = self.s.as_ref().map_or_else(|| "NULL".to_owned(), |s| format!("'{s}'"));
        let arr: Vec<String> = self.arr.iter().map(i32::to_string).collect();
        format!(
            "{}, {s}, point({}, {}), ARRAY[{}]::int[], int4range({}, {}), {}",
            self.a,
            self.p.0,
            self.p.1,
            arr.join(", "),
            self.r.0,
            self.r.1,
            self.b,
        )
    }
}

/// What the store holds: the texts of the rows by key, and the indexes by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct State {
    rows: BTreeMap<i32, Vec<Option<String>>>,
    indexes: BTreeSet<String>,
}

/// The rows and the indexes that the workload made.
#[derive(Clone, Debug, Default)]
struct Model {
    rows: BTreeMap<i32, Row>,
    indexes: BTreeSet<String>,
}

impl Model {
    fn state(&self) -> State {
        State {
            rows: self.rows.iter().map(|(&k, row)| (k, row.texts(k))).collect(),
            indexes: self.indexes.clone(),
        }
    }
}

/// What a crash file must hold.
#[derive(Clone, Debug, Default)]
struct Expect {
    acked: State,
    pending: Option<State>,
}

/// The state that the hook and the workload share.
#[derive(Debug, Default)]
struct Shared {
    armed: bool,
    expect: Expect,
    counts: Counts,
    seen: BTreeSet<u64>,
    rng_state: u64,
    failure: Option<String>,
}

/// One session on a store, over the messages of the protocol.
struct Session {
    connection: Connection,
    out: OutBuf,
}

/// The rows of an answer, each a list of texts, with `None` for NULL.
type Rows = Vec<Vec<Option<String>>>;

impl Session {
    fn new(store: &Arc<Store>) -> Result<Session, String> {
        let start = Start {
            user: "postgres".into(),
            database: "postgres".into(),
            superuser: true,
            options: None,
            settings: vec![("application_name".into(), "crash".into())],
        };
        let base = base_settings(&[]).map_err(|e| e.to_string())?;
        let settings = session_settings(&base, &start, &[]).map_err(|e| e.to_string())?;
        let mut connection = Connection::new(&start, settings, PROTOCOL_3_0);
        connection.set_store(Arc::clone(store));
        connection.set_clock(Arc::new(SimClock::new(Hlc::EPOCH_UNIX_MS + 1_000)));
        connection.set_addresses(Client::Local, Client::Local);
        let mut out = OutBuf::new();
        connection.greet(&CancelKey::new(1, PROTOCOL_3_0, [7; CANCEL_KEY_LEN]), &mut out);
        let mut session = Session { connection, out };
        session.run(&[])?;
        Ok(session)
    }

    /// Runs `input` until the session wants more, and gives the rows of the answer, or the first error.
    fn run(&mut self, input: &[u8]) -> Result<Rows, String> {
        let mut at = 0;
        loop {
            let step = self.connection.step(&input[at..], &mut self.out);
            at += step.used;
            match step.next {
                Next::Read => break,
                Next::Close => return Err("the session closed".into()),
                Next::Continue | Next::Flush => {}
            }
        }
        let answer = answer(self.out.as_bytes());
        let n = self.out.len();
        self.out.consume(n);
        answer
    }

    /// The rows of the simple query `sql`.
    fn query(&mut self, sql: &str) -> Result<Rows, String> {
        let mut input = vec![b'Q'];
        let len = u32::try_from(sql.len() + 5).map_err(|_| "a long query")?;
        input.extend_from_slice(&len.to_be_bytes());
        input.extend_from_slice(sql.as_bytes());
        input.push(0);
        self.run(&input).map_err(|e| format!("{sql}: {e}"))
    }
}

/// The rows of the messages `bytes`, or the code and the text of the first error.
fn answer(mut bytes: &[u8]) -> Result<Rows, String> {
    let mut rows = Vec::new();
    while bytes.len() >= 5 {
        let tag = bytes[0];
        let len = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
        let end = (1 + len).min(bytes.len());
        let body = &bytes[5..end];
        match tag {
            b'D' => {
                let mut at = 2;
                let mut row = Vec::new();
                let fields = u16::from_be_bytes([body[0], body[1]]);
                for _ in 0..fields {
                    let n =
                        i32::from_be_bytes([body[at], body[at + 1], body[at + 2], body[at + 3]]);
                    at += 4;
                    if let Ok(n) = usize::try_from(n) {
                        row.push(Some(String::from_utf8_lossy(&body[at..at + n]).into_owned()));
                        at += n;
                    } else {
                        row.push(None);
                    }
                }
                rows.push(row);
            }
            b'E' => {
                let mut fields = BTreeMap::new();
                for field in body.split(|&b| b == 0).filter(|f| !f.is_empty()) {
                    fields.insert(field[0], String::from_utf8_lossy(&field[1..]).into_owned());
                }
                let code = fields.get(&b'C').cloned().unwrap_or_default();
                return Err(format!("{code} {}", fields.get(&b'M').cloned().unwrap_or_default()));
            }
            _ => {}
        }
        bytes = &bytes[end..];
    }
    Ok(rows)
}

/// The store whose image and log are on `io`.
fn open(io: Arc<dyn Io>) -> Result<Arc<Store>, String> {
    Store::new().with_image(io, PathBuf::from(IMAGE)).map(Arc::new).map_err(|e| e.to_string())
}

/// The keys of the rows of the answer `rows`, in order.
fn keys(rows: &Rows) -> Result<Vec<i32>, String> {
    let mut keys = rows
        .iter()
        .map(|r| r.first().cloned().flatten().and_then(|k| k.parse().ok()).ok_or("a bad key"))
        .collect::<Result<Vec<i32>, _>>()?;
    keys.sort_unstable();
    Ok(keys)
}

/// The rows and the indexes of the store of `session`. Each index must give the rows of a scan for its query.
fn read(session: &mut Session) -> Result<State, String> {
    session.query("SET enable_indexscan = off; SET enable_bitmapscan = off")?;
    let mut state = State::default();
    for row in session.query("SELECT k, a, s, p, arr, r, b FROM t")? {
        let k = row[0].as_deref().and_then(|k| k.parse().ok()).ok_or("a bad key")?;
        state.rows.insert(k, row);
    }
    let names = session.query("SELECT indexname FROM pg_indexes WHERE tablename = 't'")?;
    state.indexes = names.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
    let present: Vec<_> = INDEXES.iter().filter(|i| state.indexes.contains(i.0)).collect();
    let mut scans = Vec::with_capacity(present.len());
    for (_, _, _, cond) in &present {
        scans.push(keys(&session.query(&format!("SELECT k FROM t WHERE {cond}"))?)?);
    }
    session.query("SET enable_seqscan = off; RESET enable_indexscan; RESET enable_bitmapscan")?;
    for ((name, _, _, cond), scan) in present.iter().zip(&scans) {
        let got = keys(&session.query(&format!("SELECT k FROM t WHERE {cond}"))?)?;
        if &got != scan {
            return Err(format!("the index {name} gives the keys {got:?}, and a scan {scan:?}"));
        }
    }
    session.query("RESET enable_seqscan")?;
    Ok(state)
}

/// A checksum of the files of `io`. Two disks with the same files and bytes have the same result.
fn disk_digest(io: &SimIo) -> u64 {
    let mut names =
        io.read_dir(Path::new(IMAGE).parent().unwrap_or(Path::new("/"))).unwrap_or_default();
    names.sort();
    let mut h = 0u64;
    for (name, _) in names {
        h = h.rotate_left(29) ^ rupg_file::checksum(name.as_bytes());
        io.read_pieces(&Path::new(IMAGE).with_file_name(&name), |piece| {
            h = h.rotate_left(17) ^ rupg_file::checksum(piece).wrapping_mul(0x9e37_79b9_7f4a_7c15);
            h ^= piece.len() as u64;
        });
    }
    h
}

/// Loads the store of one crash file and compares it with `expect`.
fn try_file(io: &SimIo, expect: &Expect) -> Result<(), String> {
    let store = open(Arc::new(io.clone()))?;
    let got = read(&mut Session::new(&store)?)?;
    let acked = expect.acked.clone();
    if got == acked || Some(&got) == expect.pending.as_ref() {
        return Ok(());
    }
    let missing: Vec<_> = acked.rows.keys().filter(|k| !got.rows.contains_key(k)).collect();
    let extra: Vec<_> = got.rows.keys().filter(|k| !acked.rows.contains_key(k)).collect();
    let changed: Vec<_> = got
        .rows
        .iter()
        .filter(|(k, r)| acked.rows.get(k).is_some_and(|a| a != *r))
        .map(|(k, _)| k)
        .collect();
    Err(format!(
        "the store does not hold the acknowledged commits: the keys {missing:?} are missing, {extra:?} are extra and {changed:?} changed, and it has the indexes {:?} where {:?} were acknowledged",
        got.indexes, acked.indexes,
    ))
}

/// Builds the crash files of the current sync point and checks each one.
fn sync_point(io: &SimIo, shared: &Mutex<Shared>, config: &Config) -> Result<(), String> {
    let (expect, mut rng, point) = {
        let mut s = lock(shared);
        s.counts.sync_points += 1;
        s.rng_state = s.rng_state.wrapping_add(1);
        let rng = SimRng::new(config.seed ^ s.rng_state.rotate_left(32));
        (s.expect.clone(), rng, s.counts.sync_points)
    };
    let base = io.fork();
    for plan in plans(&base.unsynced(), &mut rng, config.sample) {
        let file = base.fork();
        file.crash(&plan).map_err(|e| e.to_string())?;
        if !lock(shared).seen.insert(disk_digest(&file)) {
            lock(shared).counts.same += 1;
            continue;
        }
        try_file(&file, &expect)
            .map_err(|e| format!("seed {} sync point {point} plan {plan:?}: {e}", config.seed))?;
        lock(shared).counts.files += 1;
    }
    Ok(())
}

/// The workload of one seed on the store of `io`. It calls `pending` before each change that can commit, with the state after it, and `acked` after the change, with the state the store must now hold.
fn workload(
    io: &Arc<dyn Io>,
    seed: u64,
    steps: u32,
    mut see: impl FnMut(Option<&State>, &State) -> Result<(), String>,
) -> Result<(), String> {
    let mut store = open(Arc::clone(io))?;
    let mut session = Session::new(&store)?;
    let mut model = Model::default();
    let mut setup = String::from(
        "CREATE TABLE t (k int PRIMARY KEY, a int, s text, p point, arr int[], r int4range, b int);",
    );
    for (name, _, sql, _) in INDEXES {
        if !sql.is_empty() {
            let _ = write!(setup, "{sql};");
        }
        model.indexes.insert(name.to_owned());
    }
    session.query(&setup)?;
    see(None, &model.state())?;
    let mut rng = SimRng::new(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    let mut next_key = 0;
    for _ in 0..steps {
        let mut next = model.clone();
        let mut sql = String::new();
        let pick = |rng: &mut SimRng, rows: &BTreeMap<i32, Row>| {
            let n = rows.len() as u64;
            (n > 0).then(|| *rows.keys().nth(rng.below(n) as usize).unwrap_or(&0))
        };
        match rng.below(20) {
            0 => {
                store.checkpoint().map_err(|e| e.to_string())?;
                continue;
            }
            1 => {
                // A restart: the store goes, and a new one loads its image and its log.
                drop(session);
                drop(store);
                store = open(Arc::clone(io))?;
                session = Session::new(&store)?;
                continue;
            }
            2 | 3 => {
                let (name, _, make, _) = INDEXES[1 + rng.below(INDEXES.len() as u64 - 1) as usize];
                if next.indexes.remove(name) {
                    sql = format!("DROP INDEX {name}");
                } else {
                    next.indexes.insert(name.to_owned());
                    sql = make.to_owned();
                }
            }
            step => {
                // A transaction of a few changes, which rolls back at times.
                sql.push_str("BEGIN;");
                for _ in 0..1 + rng.below(4) {
                    match (rng.below(10), pick(&mut rng, &next.rows)) {
                        (0..5, _) | (_, None) => {
                            let row = Row::random(&mut rng);
                            let _ =
                                write!(sql, "INSERT INTO t VALUES ({next_key}, {});", row.sql());
                            next.rows.insert(next_key, row);
                            next_key += 1;
                        }
                        (5..8, Some(k)) => {
                            let row = Row::random(&mut rng);
                            let _ = write!(
                                sql,
                                "UPDATE t SET (a, s, p, arr, r, b) = ({}) WHERE k = {k};",
                                row.sql()
                            );
                            next.rows.insert(k, row);
                        }
                        (_, Some(k)) => {
                            let _ = write!(sql, "DELETE FROM t WHERE k = {k};");
                            next.rows.remove(&k);
                        }
                    }
                }
                if step == 19 {
                    sql.push_str("ROLLBACK");
                    session.query(&sql)?;
                    continue;
                }
                sql.push_str("COMMIT");
            }
        }
        let state = next.state();
        see(Some(&state), &model.state())?;
        session.query(&sql)?;
        model = next;
        see(None, &state)?;
    }
    Ok(())
}

/// Runs the SQL workload of one seed and checks the crash files of each sync point.
///
/// # Errors
///
/// The first crash file that does not hold what it must, or an error of the workload.
pub fn run(config: Config) -> Result<Counts, String> {
    let io = SimIo::new(config.seed);
    let shared = Arc::new(Mutex::new(Shared::default()));
    let hook: Arc<dyn Fn() + Send + Sync> = {
        let (io, shared) = (io.clone(), shared.clone());
        Arc::new(move || {
            if !lock(&shared).armed || lock(&shared).failure.is_some() {
                return;
            }
            if let Err(e) = sync_point(&io, &shared, &config) {
                lock(&shared).failure = Some(e);
            }
        })
    };
    let hooked: Arc<dyn Io> = Arc::new(Hooked { io: io.clone(), hook });
    workload(&hooked, config.seed, config.steps, |pending, acked| {
        let mut s = lock(&shared);
        if let Some(pending) = pending {
            s.expect = Expect { acked: acked.clone(), pending: Some(pending.clone()) };
        } else {
            s.armed = true;
            s.counts.commits += 1;
            s.expect = Expect { acked: acked.clone(), pending: None };
        }
        s.failure.take().map_or(Ok(()), Err)
    })?;
    // The power goes after the last step.
    sync_point(&io, &shared, &config)?;
    let counts = lock(&shared).counts;
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The planner takes each index for its query when a scan of the table is off, so the check of a crash file reads each index.
    #[test]
    fn each_index_answers_its_query() {
        let io: Arc<dyn Io> = Arc::new(SimIo::new(1));
        let store = open(io).unwrap();
        let mut session = Session::new(&store).unwrap();
        let mut setup = String::from(
            "CREATE TABLE t (k int PRIMARY KEY, a int, s text, p point, arr int[], r int4range, b int);",
        );
        for (_, _, sql, _) in INDEXES.iter().filter(|i| !i.2.is_empty()) {
            setup.push_str(sql);
            setup.push(';');
        }
        session.query(&setup).unwrap();
        let mut rng = SimRng::new(3);
        for k in 0..200 {
            session
                .query(&format!("INSERT INTO t VALUES ({k}, {})", Row::random(&mut rng).sql()))
                .unwrap();
        }
        session.query("ANALYZE t; SET enable_seqscan = off").unwrap();
        for (name, method, _, cond) in INDEXES {
            let plan = session.query(&format!("EXPLAIN SELECT k FROM t WHERE {cond}")).unwrap();
            let plan: Vec<String> =
                plan.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
            assert!(plan.iter().any(|l| l.contains(name)), "{method}: {}", plan.join("\n"));
        }
        let state = read(&mut session).unwrap();
        assert_eq!(state.rows.len(), 200);
        assert_eq!(state.indexes.len(), INDEXES.len());
    }

    /// The subset of each change: a short workload on one seed.
    #[test]
    fn each_crash_file_holds_the_acknowledged_commits_and_indexes() {
        let counts = run(Config { seed: 1, steps: 30, sample: 2 }).unwrap();
        assert!(counts.commits > 20, "{counts:?}");
        assert!(counts.files > 40, "{counts:?}");
    }
}
