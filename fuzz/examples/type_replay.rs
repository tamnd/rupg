//! Compares the corpus of `type_input` or `type_recv` with the oracle (spec/21 section 21.7).
//!
//! `type_replay sql input <corpus>` prints a psql script. For each case it sets the settings and prints the name of the file and a line: the error of `pg_input_error_info`, or the output and the hex of the send function.
//!
//! `type_replay sql recv <corpus> <dir>` writes each case to `<dir>` as a file of `COPY` in the binary format, and prints a psql script that reads each file with `COPY`. `COPY` calls the receive function of the type with the typmod of the column, as `Bind` does. The server reads the files, so `<dir>` must be an absolute path that the server can read.
//!
//! `type_replay diff input|recv <corpus> <oracle.tsv>` reads the output of the script, `psql -X -At -F '<tab>' -f script.sql`, and prints each case where rupg gives a different line. A case where rupg gives `0A000` counts as not supported. For the date and time types an input with `now`, `today`, `tomorrow` or `yesterday` depends on the clock, so only `OK` or the SQLSTATE of the error counts.

// A replay tool reads and writes files of the host, and is not part of the engine.
#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rupg_fuzz::types::{InputCase, RecvCase, decode, escape, hex, input_line, recv_line};

const FUNCTIONS: &str = r#"\set QUIET on
\set ON_ERROR_STOP on
set timezone = 'UTC';
create function pg_temp.esc(s text) returns text language sql immutable
  return replace(replace(replace(replace(s, '\', '\\'), E'\t', '\t'), E'\n', '\n'), E'\r', '\r');
create function pg_temp.err(st text, msg text, det text, hin text) returns text language sql immutable
  return pg_temp.esc('ERROR ' || st || ' ' || msg || coalesce(' DETAIL ' || nullif(det, ''), '') || coalesce(' HINT ' || nullif(hin, ''), ''));
-- The input function gets the typmod and the element type, as getTypeIOParam gives it.
create function pg_temp.inp(t text, i text) returns text language plpgsql as $$
declare
  ty pg_type := (select p from pg_type p where oid = to_regtype(t));
  io oid := case when ty.typelem <> 0 then ty.typelem else ty.oid end;
  call text;
  valid boolean;
  r text;
begin
  execute format('select pg_input_is_valid(%L, %L)', i, t) into valid;
  if not valid then
    execute format('select pg_temp.err(sql_error_code, message, detail, hint) from pg_input_error_info(%L, %L)', i, t) into r;
    return r;
  end if;
  if (select pronargs from pg_proc where oid = ty.typinput) = 1 then
    call := format('%s(%L::cstring)', ty.typinput, i);
  else
    call := format('%s(%L::cstring, %s, %s)', ty.typinput, i, io, to_regtypemod(t));
  end if;
  execute format('select ''OK '' || pg_temp.esc(%s(%s)::text) || '' '' || encode(%s(%s), ''hex'')', ty.typoutput, call, ty.typsend, call) into r;
  return r;
end
$$;
create function pg_temp.rcv(t text, path text) returns text language plpgsql as $$
declare
  ty pg_type := (select p from pg_type p where oid = to_regtype(t));
  st text;
  msg text;
  det text;
  hin text;
  r text;
begin
  drop table if exists pg_temp.r;
  execute format('create temp table r (v %s)', t);
  begin
    execute format('copy pg_temp.r from %L with (format binary)', path);
  exception when others then
    get stacked diagnostics st = returned_sqlstate, msg = message_text, det = pg_exception_detail, hin = pg_exception_hint;
    return pg_temp.err(st, msg, det, hin);
  end;
  execute format('select ''OK '' || pg_temp.esc(%s(v)::text) || '' '' || encode(%s(v), ''hex'') from pg_temp.r', ty.typoutput, ty.typsend) into r;
  return r;
end
$$;
"#;

/// The files of a corpus, by name.
fn corpus(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| entry.expect("a directory entry").path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let name = path.file_name().expect("a file name").to_string_lossy().into_owned();
            (name, std::fs::read(&path).expect("a corpus file"))
        })
        .collect()
}

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// A file of `COPY` in the binary format with one row of one column.
fn copy_file(data: &[u8]) -> Vec<u8> {
    let mut out = b"PGCOPY\n\xff\r\n\0".to_vec();
    out.extend_from_slice(&[0; 8]);
    out.extend_from_slice(&1i16.to_be_bytes());
    out.extend_from_slice(&i32::try_from(data.len()).expect("a short value").to_be_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(&(-1i16).to_be_bytes());
    out
}

fn sql(kind: &str, dir: &Path, files: Option<&Path>) -> String {
    let mut out = FUNCTIONS.to_string();
    for (name, data) in corpus(dir) {
        let call = match kind {
            "input" => {
                let Some(case) = decode::<InputCase>(&data) else { continue };
                let text = hex(case.text().as_bytes());
                writeln!(out, "{}", case.settings.sql()).unwrap();
                format!(
                    "pg_temp.inp({}, convert_from(decode('{text}', 'hex'), 'UTF8'))",
                    literal(case.ty().name)
                )
            }
            _ => {
                let Some(case) = decode::<RecvCase>(&data) else { continue };
                let files = files.expect("a directory for the files of COPY");
                let path = files.join(format!("{name}.copy"));
                std::fs::write(&path, copy_file(&case.data)).expect("a file of COPY");
                let path = path.to_str().expect("a UTF-8 path");
                format!("pg_temp.rcv({}, {})", literal(case.ty().name), literal(path))
            }
        };
        writeln!(out, "select {}, {call};", literal(&name)).unwrap();
    }
    out
}

/// The words of a line that the comparison uses. For an input that depends on the clock, only `OK` or the error and its SQLSTATE count.
fn key(line: &str, clock: bool) -> &str {
    if !clock {
        return line;
    }
    if line.starts_with("OK ") {
        return "OK";
    }
    match line.match_indices(' ').nth(1) {
        Some((at, _)) => &line[..at],
        None => line,
    }
}

const CLOCK_WORDS: [&str; 4] = ["now", "today", "tomorrow", "yesterday"];

fn diff(kind: &str, dir: &Path, oracle: &Path) -> bool {
    let text = std::fs::read_to_string(oracle).expect("the output of the oracle");
    let oracle: BTreeMap<&str, &str> = text.lines().filter_map(|l| l.split_once('\t')).collect();
    let (mut same, mut not_yet, mut differ, mut missing) = (0, 0, 0, 0);
    for (name, data) in corpus(dir) {
        let (ty, input, ours, clock) = match kind {
            "input" => {
                let Some(case) = decode::<InputCase>(&data) else { continue };
                let lower = case.text().to_lowercase();
                let clock = case.ty().name.contains("time") || case.ty().name.contains("date");
                let clock = clock && CLOCK_WORDS.iter().any(|w| lower.contains(w));
                let input = format!("{} {}", case.settings.sql(), escape(case.text()));
                (case.ty().name, input, input_line(&case), clock)
            }
            _ => {
                let Some(case) = decode::<RecvCase>(&data) else { continue };
                (case.ty().name, hex(&case.data), recv_line(&case), false)
            }
        };
        let Some(theirs) = oracle.get(name.as_str()) else {
            missing += 1;
            continue;
        };
        if ours.starts_with("ERROR 0A000 ") {
            not_yet += 1;
        } else if key(&ours, clock) == key(theirs, clock) {
            same += 1;
        } else {
            differ += 1;
            println!("{name} {ty} {input}\n  oracle: {theirs}\n  rupg:   {ours}");
        }
    }
    println!(
        "{same} same, {differ} different, {not_yet} not supported, {missing} not in the oracle output"
    );
    differ == 0 && missing == 0
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args[..] {
        ["sql", kind @ ("input" | "recv"), dir, ref rest @ ..] => {
            let files = rest.first().map(PathBuf::from);
            if kind == "recv" && files.is_none() {
                eprintln!("type_replay sql recv needs a directory for the files of COPY");
                return ExitCode::FAILURE;
            }
            print!("{}", sql(kind, Path::new(dir), files.as_deref()));
            ExitCode::SUCCESS
        }
        ["diff", kind @ ("input" | "recv"), dir, oracle] => {
            if diff(kind, Path::new(dir), Path::new(oracle)) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        _ => {
            eprintln!(
                "usage: type_replay sql input <corpus>\n       type_replay sql recv <corpus> <dir>\n       type_replay diff input|recv <corpus> <oracle.tsv>"
            );
            ExitCode::FAILURE
        }
    }
}
