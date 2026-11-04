//! YCSB in the process: the workloads of `rupg-bench ycsb` on sessions of rupg that run in the threads of this program, with no socket.
//!
//! Each thread is one client with its own session on one store in memory. A client sends the messages `Bind`, `Execute` and `Sync` of the extended protocol for a prepared statement, as a client of the server sends them, and reads the answer from the output buffer of its session. So the run measures the engine and the protocol, and not the socket and the context switch. The store in memory has no durability, as DuckDB with `:memory:`.
//!
//! The keys, the values and the scrambled zipfian distribution are those of `rupg-bench`, in `src/ycsb.rs`. A read must give one row and an update must change one row, or the operation counts as an error.
//!
//! ```text
//! cargo run --release -p rupg-session --example ycsb -- --records 100000 --threads 1,6 --workloads c,a --time 10
//! ```

// A benchmark program and not a part of the engine: it takes its time, its threads and `/proc` from `std`.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use rupg_session::connection::{Client, Connection, Next, Start, base_settings, session_settings};
use rupg_session::store::Store;
use rupg_wire::{CANCEL_KEY_LEN, CancelKey, OutBuf};

/// mimalloc, the allocator of the rupg binary, so that the run measures what a server does.
#[global_allocator]
static ALLOCATOR: rupg_platform::alloc::Counting = rupg_platform::alloc::Counting;

const FIELDS: usize = 10;
const FIELD_LENGTH: usize = 100;
const PROTOCOL_3_0: u32 = 3 << 16;
/// The item count and its zeta of the scrambled zipfian generator of YCSB.
const SCRAMBLED_ITEMS: u64 = 10_000_000_000;
const SCRAMBLED_ZETAN: f64 = 26.469_028_201_783_02;
const ZIPFIAN_CONSTANT: f64 = 0.99;

/// `Utils.fnvhash64` of YCSB.
fn fnv_hash64(mut v: u64) -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for _ in 0..8 {
        h ^= v & 0xff;
        v >>= 8;
        h = h.wrapping_mul(1_099_511_628_211);
    }
    (h.cast_signed()).unsigned_abs()
}

fn key_name(keynum: u64) -> String {
    format!("user{}", fnv_hash64(keynum))
}

/// SplitMix64, as in `rupg-bench`.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    #[allow(clippy::cast_precision_loss)]
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn fill_text(&mut self, out: &mut [u8]) {
        const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        for chunk in out.chunks_mut(8) {
            let mut r = self.next_u64();
            for b in chunk {
                *b = CHARS[usize::try_from(r % CHARS.len() as u64).unwrap_or(0)];
                r /= CHARS.len() as u64;
            }
        }
    }
}

/// The `ScrambledZipfianGenerator` of YCSB over `records` records.
struct Zipfian {
    records: u64,
    alpha: f64,
    eta: f64,
    theta: f64,
}

impl Zipfian {
    #[allow(clippy::cast_precision_loss)]
    fn new(records: u64) -> Zipfian {
        let theta = ZIPFIAN_CONSTANT;
        let zeta2 = 1.0 + 1.0 / 2f64.powf(theta);
        let items = (SCRAMBLED_ITEMS + 1) as f64;
        let eta = (1.0 - (2.0 / items).powf(1.0 - theta)) / (1.0 - zeta2 / SCRAMBLED_ZETAN);
        Zipfian { records, alpha: 1.0 / (1.0 - theta), eta, theta }
    }

    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn next(&self, rng: &mut Rng) -> u64 {
        let u = rng.next_f64();
        let uz = u * SCRAMBLED_ZETAN;
        let rank = if uz < 1.0 {
            0
        } else if uz < 1.0 + 0.5f64.powf(self.theta) {
            1
        } else {
            let items = SCRAMBLED_ITEMS + 1;
            ((items as f64 * (self.eta * u - self.eta + 1.0).powf(self.alpha)) as u64)
                .min(items - 1)
        };
        fnv_hash64(rank) % self.records
    }
}

/// A message of the client: the type byte, the length and the body.
fn message(out: &mut Vec<u8>, tag: u8, body: &[u8]) {
    out.push(tag);
    out.extend_from_slice(&u32::try_from(body.len() + 4).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(body);
}

fn query(sql: &str) -> Vec<u8> {
    let mut out = Vec::new();
    message(&mut out, b'Q', format!("{sql}\0").as_bytes());
    out
}

fn parse(name: &str, sql: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut body = format!("{name}\0{sql}\0").into_bytes();
    body.extend_from_slice(&0i16.to_be_bytes());
    message(&mut out, b'P', &body);
    message(&mut out, b'S', &[]);
    out
}

/// `Bind` of the unnamed portal to the statement `name` with text parameters, `Execute` and `Sync`.
fn bind_execute(out: &mut Vec<u8>, name: &str, params: &[&[u8]]) {
    let mut body = Vec::with_capacity(64);
    body.push(0);
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&i16::try_from(params.len()).unwrap_or(0).to_be_bytes());
    for p in params {
        body.extend_from_slice(&i32::try_from(p.len()).unwrap_or(0).to_be_bytes());
        body.extend_from_slice(p);
    }
    body.extend_from_slice(&0i16.to_be_bytes());
    message(out, b'B', &body);
    message(out, b'E', &[0, 0, 0, 0, 0]);
    message(out, b'S', &[]);
}

/// The answer to one input: the rows, the last command tag and the first error.
#[derive(Default)]
struct Answer {
    rows: usize,
    tag: Vec<u8>,
    error: Option<String>,
}

/// One client: a session and its buffers.
struct Session {
    connection: Connection,
    out: OutBuf,
    input: Vec<u8>,
}

impl Session {
    fn new(store: &Arc<Store>, pid: i32) -> Session {
        let start = Start {
            user: "postgres".into(),
            database: "postgres".into(),
            superuser: true,
            options: None,
            settings: vec![("application_name".into(), "ycsb".into())],
        };
        let base = base_settings(&[]).expect("the default settings");
        let settings = session_settings(&base, &start, &[]).expect("the settings of the session");
        let mut connection = Connection::new(&start, settings, PROTOCOL_3_0);
        connection.set_store(Arc::clone(store));
        connection.set_addresses(Client::Local, Client::Local);
        let mut out = OutBuf::new();
        connection.greet(&CancelKey::new(pid, PROTOCOL_3_0, [7; CANCEL_KEY_LEN]), &mut out);
        let mut session = Session { connection, out, input: Vec::new() };
        session.run();
        session
    }

    /// Runs the input until the session wants more, and reads the answer.
    fn run(&mut self) -> Answer {
        let mut at = 0;
        loop {
            let step = self.connection.step(&self.input[at..], &mut self.out);
            at += step.used;
            match step.next {
                Next::Read => break,
                Next::Close => panic!("the session closed"),
                Next::Continue | Next::Flush => {}
            }
        }
        assert_eq!(at, self.input.len(), "the session left input");
        self.input.clear();
        let answer = read_answer(self.out.as_bytes());
        let n = self.out.len();
        self.out.consume(n);
        answer
    }

    fn send(&mut self, input: &[u8]) -> Answer {
        self.input.extend_from_slice(input);
        self.run()
    }
}

fn read_answer(mut bytes: &[u8]) -> Answer {
    let mut answer = Answer::default();
    while bytes.len() >= 5 {
        let tag = bytes[0];
        let len = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
        let body = &bytes[5..(1 + len).min(bytes.len())];
        match tag {
            b'D' => answer.rows += 1,
            b'C' => answer.tag = body.strip_suffix(&[0]).unwrap_or(body).to_vec(),
            b'E' if answer.error.is_none() => {
                answer.error = Some(String::from_utf8_lossy(body).replace('\0', " "));
            }
            _ => {}
        }
        bytes = &bytes[(1 + len).min(bytes.len())..];
    }
    answer
}

fn load(store: &Arc<Store>, records: u64, seed: u64) {
    let mut s = Session::new(store, 1);
    let fields: Vec<String> = (0..FIELDS).map(|i| format!("field{i} TEXT")).collect();
    let create = format!(
        "DROP TABLE IF EXISTS usertable; CREATE TABLE usertable (ycsb_key VARCHAR(255) PRIMARY KEY, {})",
        fields.join(", ")
    );
    let answer = s.send(&query(&create));
    assert!(answer.error.is_none(), "{create}: {:?}", answer.error);
    let mut rng = Rng(seed);
    let mut field = [0u8; FIELD_LENGTH];
    let mut n = 0;
    while n < records {
        let mut sql = String::from("INSERT INTO usertable VALUES ");
        for i in n..(n + 1000).min(records) {
            if i > n {
                sql.push(',');
            }
            sql.push_str("('");
            sql.push_str(&key_name(i));
            sql.push('\'');
            for _ in 0..FIELDS {
                rng.fill_text(&mut field);
                sql.push_str(",'");
                sql.push_str(std::str::from_utf8(&field).unwrap_or_default());
                sql.push('\'');
            }
            sql.push(')');
        }
        let answer = s.send(&query(&sql));
        assert!(answer.error.is_none(), "load: {:?}", answer.error);
        n = (n + 1000).min(records);
    }
    let answer = s.send(&query("ANALYZE usertable"));
    assert!(answer.error.is_none(), "analyze: {:?}", answer.error);
}

/// The result of one client.
#[derive(Default)]
struct Tally {
    ops: u64,
    errors: u64,
    first_error: Option<String>,
    reads: Vec<u32>,
    updates: Vec<u32>,
}

fn client(store: &Arc<Store>, pid: i32, records: u64, read_share: f64, until: Instant) -> Tally {
    let mut s = Session::new(store, pid);
    let fields: Vec<String> = (0..FIELDS).map(|i| format!("field{i}")).collect();
    let mut prepare = parse(
        "read",
        &format!("SELECT ycsb_key, {} FROM usertable WHERE ycsb_key = $1", fields.join(", ")),
    );
    for (i, f) in fields.iter().enumerate() {
        prepare.extend(parse(
            &format!("update{i}"),
            &format!("UPDATE usertable SET {f} = $1 WHERE ycsb_key = $2"),
        ));
    }
    let answer = s.send(&prepare);
    assert!(answer.error.is_none(), "prepare: {:?}", answer.error);
    let names: Vec<String> = (0..FIELDS).map(|i| format!("update{i}")).collect();
    let keys = Zipfian::new(records);
    let mut rng = Rng(0x5EED_0000 + u64::try_from(pid).unwrap_or(0));
    let mut tally = Tally::default();
    let mut value = [0u8; FIELD_LENGTH];
    let mut input = Vec::with_capacity(256);
    let mut checked = 0u32;
    let mut now = Instant::now();
    loop {
        // The clock is read once for each operation, for its latency, and the end is tested on the same reading.
        if checked == 0 && now >= until {
            break;
        }
        checked = (checked + 1) % 64;
        let key = key_name(keys.next(&mut rng));
        let read = rng.next_f64() < read_share;
        input.clear();
        if read {
            bind_execute(&mut input, "read", &[key.as_bytes()]);
        } else {
            rng.fill_text(&mut value);
            let field = usize::try_from(rng.next_u64() % FIELDS as u64).unwrap_or(0);
            bind_execute(&mut input, &names[field], &[&value, key.as_bytes()]);
        }
        let answer = s.send(&input);
        let end = Instant::now();
        let nanos = u32::try_from(end.duration_since(now).as_nanos()).unwrap_or(u32::MAX);
        now = end;
        let good = answer.error.is_none()
            && if read { answer.rows == 1 } else { answer.tag == b"UPDATE 1" };
        if !good {
            tally.errors += 1;
            if tally.first_error.is_none() {
                tally.first_error =
                    Some(answer.error.unwrap_or_else(|| format!("{} rows", answer.rows)));
            }
        }
        tally.ops += 1;
        if read { tally.reads.push(nanos) } else { tally.updates.push(nanos) }
    }
    tally
}

/// The user and system CPU time of the process, from `/proc/self/stat`, in clock ticks of 10 ms.
fn cpu_ticks() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let after = stat.rsplit_once(')').map_or("", |(_, rest)| rest);
    let fields: Vec<&str> = after.split_whitespace().collect();
    let tick = |i: usize| fields.get(i).and_then(|f| f.parse::<u64>().ok()).unwrap_or(0);
    tick(11) + tick(12)
}

fn percentile(sorted: &[u32], p: f64) -> String {
    if sorted.is_empty() {
        return "-".into();
    }
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let at = ((sorted.len() - 1) as f64 * p) as usize;
    format!("{:.1}", f64::from(sorted[at]) / 1000.0)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |name: &str, default: &str| -> String {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
            .unwrap_or_else(|| default.to_owned())
    };
    let records: u64 = value("--records", "100000").parse().expect("--records N");
    let time: u64 = value("--time", "10").parse().expect("--time SECONDS");
    let seed: u64 = value("--seed", "1").parse().expect("--seed N");
    let threads: Vec<u32> =
        value("--threads", "1,6").split(',').map(|t| t.parse().expect("--threads N,M")).collect();
    let workloads = value("--workloads", "c,a");
    // With more than one store, client `i` uses store `i % stores`, each store with its own copy of the records. So a run can tell the cost of what the sessions of one store share from the cost of the machine.
    let stores: usize = value("--stores", "1").parse().expect("--stores N");
    rupg_platform::alloc::tune();
    let stores: Vec<Arc<Store>> = (0..stores.max(1)).map(|_| Arc::new(Store::new())).collect();
    let started = Instant::now();
    let loads: Vec<_> = stores
        .iter()
        .map(|store| {
            let store = Arc::clone(store);
            std::thread::Builder::new()
                .stack_size(64 << 20)
                .spawn(move || load(&store, records, seed))
                .expect("a thread")
        })
        .collect();
    for handle in loads {
        handle.join().expect("the load");
    }
    println!(
        "load {records} records into {} stores in {:.1} s",
        stores.len(),
        started.elapsed().as_secs_f64()
    );
    for workload in workloads.split(',') {
        let read_share = match workload {
            "c" => 1.0,
            "b" => 0.95,
            "a" => 0.5,
            _ => panic!("the workloads are a, b and c"),
        };
        for &count in &threads {
            let until = Instant::now() + Duration::from_secs(time);
            let cpu = cpu_ticks();
            let begin = Instant::now();
            let handles: Vec<_> = (0..count)
                .map(|i| {
                    let store = Arc::clone(&stores[usize::try_from(i).unwrap_or(0) % stores.len()]);
                    let pid = 100 + i32::try_from(i).unwrap_or(0);
                    std::thread::Builder::new()
                        .stack_size(16 << 20)
                        .spawn(move || client(&store, pid, records, read_share, until))
                        .expect("a thread")
                })
                .collect();
            let tallies: Vec<Tally> =
                handles.into_iter().map(|h| h.join().expect("a client")).collect();
            let seconds = begin.elapsed().as_secs_f64();
            let cpu = cpu_ticks() - cpu;
            let ops: u64 = tallies.iter().map(|t| t.ops).sum();
            let errors: u64 = tallies.iter().map(|t| t.errors).sum();
            let mut reads: Vec<u32> =
                tallies.iter().flat_map(|t| t.reads.iter().copied()).collect();
            let mut updates: Vec<u32> =
                tallies.iter().flat_map(|t| t.updates.iter().copied()).collect();
            reads.sort_unstable();
            updates.sort_unstable();
            #[allow(clippy::cast_precision_loss)]
            let per_s = ops as f64 / seconds;
            #[allow(clippy::cast_precision_loss)]
            let cpu_per_op = cpu as f64 * 10_000.0 / ops.max(1) as f64;
            println!(
                "{workload} {count:>3} threads {per_s:>10.0} ops/s  read p50 {} p99 {} us  update p50 {} p99 {} us  cpu {cpu_per_op:.1} us/op  errors {errors}{}",
                percentile(&reads, 0.5),
                percentile(&reads, 0.99),
                percentile(&updates, 0.5),
                percentile(&updates, 0.99),
                tallies
                    .iter()
                    .find_map(|t| t.first_error.as_deref())
                    .map(|e| format!(" ({e})"))
                    .unwrap_or_default(),
            );
        }
    }
}
