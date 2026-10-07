//! The configuration parameters of PostgreSQL, from `guc_parameters.dat` and `guc_tables.c`.
//!
//! `guc_parameters.dat` has one entry for each parameter. Most values in it are literals, but some boot values and limits are C expressions over macros of the build, for example `MAX_KILOBYTES` or `(8 * 1024 * 1024) / BLCKSZ`, and an enum names its options array and its boot value as a C constant. `guc_tables.c` holds 34 of the options arrays and the text of each group. The other five arrays are in files that rupg does not vendor, so they are copied below. The values of the macros and the `#ifdef` lines are the ones of the Linux build of the oracle (spec/21 section 21.3.3), because the table test compares `pg_settings` with that server.
//!
//! `cargo xtask guc [--check]` writes `crates/rupg-session/src/generated/guc.rs`. Lifted from `xtask/src/postgres/guc.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use std::path::Path;

use crate::dat::dat_entries;
use crate::read;

const DAT: &str = "vendor/postgres-19/src/backend/utils/misc/guc_parameters.dat";
const TABLES: &str = "vendor/postgres-19/src/backend/utils/misc/guc_tables.c";
const OUTPUT: &str = "crates/rupg-session/src/generated/guc.rs";

pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let check = args.iter().any(|a| a == "--check");
    if let Some(other) = args.iter().find(|a| *a != "--check") {
        return Err(format!("guc: unknown argument {other:?}"));
    }
    let text = guc(&[read(&root.join(DAT))?, read(&root.join(TABLES))?])?;
    let file = root.join(OUTPUT);
    if std::fs::read_to_string(&file).unwrap_or_default() == text {
        println!("guc: {OUTPUT} matches the vendored files");
        return Ok(());
    }
    if check {
        return Err(format!("guc: {OUTPUT} is not up to date, run `cargo xtask guc`"));
    }
    std::fs::write(&file, text).map_err(|e| format!("could not write {OUTPUT}: {e}"))?;
    println!("guc: wrote {OUTPUT}");
    Ok(())
}

/// The macros of the oracle build that the `#if` lines of the options arrays test. The oracle is a Linux build on x86-64 with OpenSSL and ICU, and without lz4, zstd and liburing.
const DEFINED: [&str; 10] = [
    "HAVE_SYSLOG",
    "HAVE_SYNCFS",
    "HAVE_COPY_FILE_RANGE",
    "HAVE_POSIX_FALLOCATE",
    "PG_INSTR_TSC_CLOCK",
    "O_SYNC",
    "O_DSYNC",
    "USE_DSM_POSIX",
    "USE_DSM_SYSV",
    "USE_DSM_MMAP",
];

/// The macros in the boot values and the limits, with their values in the oracle build. A value can use other macros.
const MACROS: [(&str, &str); 80] = [
    ("BLCKSZ", "8192"),
    ("DBL_MAX", "1.7976931348623157e308"),
    ("DEFAULT_ASSERT_ENABLED", "false"),
    ("DEFAULT_BACKEND_FLUSH_AFTER", "0"),
    ("DEFAULT_BGWRITER_FLUSH_AFTER", "64"),
    ("DEFAULT_CHECKPOINT_FLUSH_AFTER", "32"),
    ("DEFAULT_CPU_INDEX_TUPLE_COST", "0.005"),
    ("DEFAULT_CPU_OPERATOR_COST", "0.0025"),
    ("DEFAULT_CPU_TUPLE_COST", "0.01"),
    ("DEFAULT_CURSOR_TUPLE_FRACTION", "0.1"),
    ("DEFAULT_DEBUG_COPY_PARSE_PLAN_TREES", "false"),
    ("DEFAULT_DEBUG_DISCARD_CACHES", "0"),
    ("DEFAULT_DEBUG_RAW_EXPRESSION_COVERAGE_TEST", "false"),
    ("DEFAULT_DEBUG_WRITE_READ_PARSE_PLAN_TREES", "false"),
    ("DEFAULT_DYNAMIC_SHARED_MEMORY_TYPE", "DSM_IMPL_POSIX"),
    ("DEFAULT_EFFECTIVE_CACHE_SIZE", "524288"),
    ("DEFAULT_EFFECTIVE_IO_CONCURRENCY", "16"),
    ("DEFAULT_EVENT_SOURCE", "\"PostgreSQL\""),
    ("DEFAULT_FILE_EXTEND_METHOD", "FILE_EXTEND_METHOD_POSIX_FALLOCATE"),
    ("DEFAULT_GEQO_EFFORT", "5"),
    ("DEFAULT_GEQO_SELECTION_BIAS", "2.0"),
    ("DEFAULT_IO_COMBINE_LIMIT", "Min(MAX_IO_COMBINE_LIMIT, (128 * 1024) / BLCKSZ)"),
    ("DEFAULT_IO_METHOD", "IOMETHOD_WORKER"),
    ("DEFAULT_MAINTENANCE_IO_CONCURRENCY", "16"),
    ("DEFAULT_MAX_WAL_SEGS", "64"),
    ("DEFAULT_MIN_WAL_SEGS", "5"),
    ("DEFAULT_PARALLEL_SETUP_COST", "1000.0"),
    ("DEFAULT_PARALLEL_TUPLE_COST", "0.1"),
    ("DEFAULT_PGSOCKET_DIR", "\"/tmp\""),
    ("DEFAULT_RANDOM_PAGE_COST", "4.0"),
    ("DEFAULT_RECURSIVE_WORKTABLE_FACTOR", "10.0"),
    ("DEFAULT_SEQ_PAGE_COST", "1.0"),
    ("DEFAULT_SHARED_MEMORY_TYPE", "SHMEM_TYPE_MMAP"),
    ("DEFAULT_SSL_CIPHERS", "\"HIGH:MEDIUM:+3DES:!aNULL\""),
    ("DEFAULT_SSL_GROUPS", "\"X25519:prime256v1\""),
    ("DEFAULT_SYSLOG_FACILITY", "LOG_LOCAL0"),
    ("DEFAULT_TABLE_ACCESS_METHOD", "\"heap\""),
    ("DEFAULT_TOAST_COMPRESSION", "TOAST_PGLZ_COMPRESSION"),
    ("DEFAULT_UPDATE_PROCESS_TITLE", "true"),
    ("DEFAULT_WAL_SYNC_METHOD", "WAL_SYNC_METHOD_FDATASYNC"),
    ("DEFAULT_WAL_WRITER_FLUSH_AFTER", "((1024 * 1024) / XLOG_BLCKSZ)"),
    ("DEFAULT_XLOG_SEG_SIZE", "(16 * 1024 * 1024)"),
    ("DEF_PGPORT", "5432"),
    ("EXEC_BACKEND_ENABLED", "false"),
    ("FUNC_MAX_ARGS", "100"),
    ("FirstNormalObjectId", "16384"),
    ("HOURS_PER_DAY", "24"),
    ("INDEX_MAX_KEYS", "32"),
    ("INT_MAX", "2147483647"),
    ("INT_MIN", "(-2147483647 - 1)"),
    ("MAX_BACKENDS", "262143"),
    ("MAX_BAS_VAC_RING_SIZE_KB", "(16 * 1024 * 1024)"),
    ("MAX_DEBUG_DISCARD_CACHES", "0"),
    ("MAX_GEQO_EFFORT", "10"),
    ("MAX_GEQO_SELECTION_BIAS", "2.0"),
    ("MAX_IO_COMBINE_LIMIT", "128"),
    ("MAX_IO_CONCURRENCY", "1000"),
    ("MAX_IO_WORKERS", "32"),
    ("MAX_KILOBYTES", "INT_MAX"),
    ("MAX_PARALLEL_WORKER_LIMIT", "1024"),
    ("MAX_STATISTICS_TARGET", "10000"),
    ("MINS_PER_HOUR", "60"),
    ("MIN_DEBUG_DISCARD_CACHES", "0"),
    ("MIN_GEQO_EFFORT", "1"),
    ("MIN_GEQO_SELECTION_BIAS", "1.5"),
    ("MaxAllocSize", "1073741823"),
    ("NAMEDATALEN", "64"),
    ("PG_KRB_SRVTAB", "\"\""),
    ("PG_VERSION", "\"19beta4\""),
    ("PG_VERSION_NUM", "190000"),
    ("RELSEG_SIZE", "131072"),
    ("SCRAM_SHA_256_DEFAULT_ITERATIONS", "4096"),
    ("SECS_PER_MINUTE", "60"),
    ("SIZE_MAX", "18446744073709551615"),
    ("SLRU_MAX_ALLOWED_BUFFERS", "((1024 * 1024 * 1024) / BLCKSZ)"),
    ("SSL_LIBRARY", "\"OpenSSL\""),
    ("WRITEBACK_MAX_PENDING_FLUSHES", "256"),
    ("WalSegMaxSize", "1024 * 1024 * 1024"),
    ("WalSegMinSize", "1024 * 1024"),
    ("XLOG_BLCKSZ", "8192"),
];

/// The options arrays that are not in `guc_tables.c`, as they are in their files at the pin.
const OTHER_ARRAYS: &str = r#"
/* src/backend/access/transam/xlog.c */
const struct config_enum_entry wal_sync_method_options[] = {
	{"fsync", WAL_SYNC_METHOD_FSYNC, false},
#ifdef HAVE_FSYNC_WRITETHROUGH
	{"fsync_writethrough", WAL_SYNC_METHOD_FSYNC_WRITETHROUGH, false},
#endif
	{"fdatasync", WAL_SYNC_METHOD_FDATASYNC, false},
#ifdef O_SYNC
	{"open_sync", WAL_SYNC_METHOD_OPEN, false},
#endif
#ifdef O_DSYNC
	{"open_datasync", WAL_SYNC_METHOD_OPEN_DSYNC, false},
#endif
	{NULL, 0, false}
};

/* src/backend/access/transam/xlog.c */
const struct config_enum_entry archive_mode_options[] = {
	{"always", ARCHIVE_MODE_ALWAYS, false},
	{"on", ARCHIVE_MODE_ON, false},
	{"off", ARCHIVE_MODE_OFF, false},
	{"true", ARCHIVE_MODE_ON, true},
	{"false", ARCHIVE_MODE_OFF, true},
	{"yes", ARCHIVE_MODE_ON, true},
	{"no", ARCHIVE_MODE_OFF, true},
	{"1", ARCHIVE_MODE_ON, true},
	{"0", ARCHIVE_MODE_OFF, true},
	{NULL, 0, false}
};

/* src/backend/storage/aio/aio.c */
const struct config_enum_entry io_method_options[] = {
	{"sync", IOMETHOD_SYNC, false},
	{"worker", IOMETHOD_WORKER, false},
#ifdef IOMETHOD_IO_URING_ENABLED
	{"io_uring", IOMETHOD_IO_URING, false},
#endif
	{NULL, 0, false}
};

/* src/backend/access/transam/xlogrecovery.c */
const struct config_enum_entry recovery_target_action_options[] = {
	{"pause", RECOVERY_TARGET_ACTION_PAUSE, false},
	{"promote", RECOVERY_TARGET_ACTION_PROMOTE, false},
	{"shutdown", RECOVERY_TARGET_ACTION_SHUTDOWN, false},
	{NULL, 0, false}
};

/* src/backend/access/rmgrdesc/xlogdesc.c */
const struct config_enum_entry wal_level_options[] = {
	{"minimal", WAL_LEVEL_MINIMAL, false},
	{"replica", WAL_LEVEL_REPLICA, false},
	{"archive", WAL_LEVEL_REPLICA, true},	/* deprecated */
	{"hot_standby", WAL_LEVEL_REPLICA, true},	/* deprecated */
	{"logical", WAL_LEVEL_LOGICAL, false},
	{NULL, 0, false}
};

/* src/backend/storage/ipc/dsm_impl.c */
const struct config_enum_entry dynamic_shared_memory_options[] = {
#ifdef USE_DSM_POSIX
	{"posix", DSM_IMPL_POSIX, false},
#endif
#ifdef USE_DSM_SYSV
	{"sysv", DSM_IMPL_SYSV, false},
#endif
#ifdef USE_DSM_WINDOWS
	{"windows", DSM_IMPL_WINDOWS, false},
#endif
#ifdef USE_DSM_MMAP
	{"mmap", DSM_IMPL_MMAP, false},
#endif
	{NULL, 0, false}
};
"#;

/// One entry of an options array: the name, the C constant and the hidden mark.
struct Choice {
    name: String,
    constant: String,
    hidden: bool,
}

/// One parameter, as the generated file spells it.
struct Row {
    key: String,
    name: String,
    context: &'static str,
    category: String,
    short: String,
    extra: String,
    flags: String,
    kind: String,
}

/// Renders the parameter table from the text of `guc_parameters.dat` and `guc_tables.c`.
fn guc(inputs: &[String]) -> Result<String, String> {
    let [dat, tables] = inputs else {
        return Err("guc: give guc_parameters.dat and guc_tables.c".to_string());
    };
    let tables = preprocess(tables)?;
    let mut arrays = options_arrays(&tables)?;
    arrays.extend(options_arrays(&preprocess(OTHER_ARRAYS)?)?);
    let groups = group_names(&tables)?;
    let macros: BTreeMap<&str, &str> = MACROS.into_iter().collect();

    let mut rows = Vec::new();
    let mut used = BTreeMap::new();
    for entry in dat_entries("guc_parameters.dat", dat)? {
        let field = |key: &str| entry.get(key).map(String::as_str);
        let name = field("name").ok_or("guc_parameters.dat: an entry with no name")?;
        let bad = |what: &str| format!("guc_parameters.dat: {name} has a bad {what}");
        // A parameter behind `ifdef` is only in a build with a debug macro, and the oracle has none of them.
        if field("ifdef").is_some() {
            continue;
        }
        let context = match field("context") {
            Some("PGC_INTERNAL") => "Internal",
            Some("PGC_POSTMASTER") => "Postmaster",
            Some("PGC_SIGHUP") => "Sighup",
            Some("PGC_SU_BACKEND") => "SuperuserBackend",
            Some("PGC_BACKEND") => "Backend",
            Some("PGC_SUSET") => "Superuser",
            Some("PGC_USERSET") => "User",
            _ => return Err(bad("context")),
        };
        let group = field("group").ok_or_else(|| bad("group"))?;
        let category = groups.get(group).ok_or_else(|| bad("group"))?;
        let mut flags = Vec::new();
        for flag in field("flags").unwrap_or("").split('|').map(str::trim) {
            match flag.strip_prefix("GUC_") {
                Some(flag) if !flag.is_empty() => flags.push(format!("flag::{flag}")),
                None if flag.is_empty() => {}
                _ => return Err(bad("flag")),
            }
        }
        let flags = if flags.is_empty() { "0".to_string() } else { flags.join(" | ") };
        let boot = field("boot_val").ok_or_else(|| bad("boot_val"))?;
        let number = |key: &str| {
            let text = field(key).ok_or_else(|| bad(key))?;
            eval(text, &macros).map_err(|e| format!("{}: {e}", bad(key)))
        };
        let kind = match field("type") {
            Some("bool") => {
                let boot = match macros.get(boot).copied().unwrap_or(boot) {
                    "true" => "true",
                    "false" => "false",
                    _ => return Err(bad("boot_val")),
                };
                format!("Kind::Bool {{ boot: {boot} }}")
            }
            Some("int") => {
                let int = |key: &str| -> Result<i32, String> {
                    let value = number(key)?;
                    if value.fract() != 0.0
                        || value < f64::from(i32::MIN)
                        || value > f64::from(i32::MAX)
                    {
                        return Err(bad(key));
                    }
                    Ok(value as i32)
                };
                let (boot, min, max) = (int("boot_val")?, int("min")?, int("max")?);
                format!("Kind::Int {{ boot: {boot}, min: {min}, max: {max} }}")
            }
            Some("real") => {
                let (boot, min, max) = (number("boot_val")?, number("min")?, number("max")?);
                format!("Kind::Real {{ boot: {boot:?}, min: {min:?}, max: {max:?} }}")
            }
            Some("string") => match boot {
                "NULL" => "Kind::String { boot: None }".to_string(),
                boot => {
                    let text = macros.get(boot).copied().unwrap_or(boot);
                    let value = c_string(text).ok_or_else(|| bad("boot_val"))?;
                    format!("Kind::String {{ boot: Some({value:?}) }}")
                }
            },
            Some("enum") => {
                // `ssl_max_protocol_version` takes its array without the first entry.
                let options = field("options").ok_or_else(|| bad("options"))?;
                let (options, skip) = match options.split_once(" + ") {
                    Some((options, "1")) => (options, 1),
                    None => (options, 0),
                    Some(_) => return Err(bad("options")),
                };
                let choices = arrays.get(options).ok_or_else(|| bad("options"))?;
                // The values come from the whole array, as the static array of options has them.
                let all = values(choices);
                let kept = all.get(skip..).ok_or_else(|| bad("options"))?;
                let constant = macros.get(boot).copied().unwrap_or(boot);
                let value = kept
                    .iter()
                    .find(|(c, _)| c == constant)
                    .map(|&(_, v)| v)
                    .ok_or_else(|| bad("boot_val"))?;
                let array = options.to_ascii_uppercase();
                used.insert(options.to_string(), ());
                match skip {
                    0 => format!("Kind::Enum {{ boot: {value}, options: &{array} }}"),
                    _ => format!(
                        "Kind::Enum {{ boot: {value}, options: {array}.split_at({skip}).1 }}"
                    ),
                }
            }
            _ => return Err(bad("type")),
        };
        // `gen_guc_tables.pl` writes a description into C as it is, with `\` before a `"`.
        let short = field("short_desc").and_then(unescape).ok_or_else(|| bad("short_desc"))?;
        let extra = match field("long_desc") {
            Some(text) => format!("Some({:?})", unescape(text).ok_or_else(|| bad("long_desc"))?),
            None => "None".to_string(),
        };
        rows.push(Row {
            key: name.to_ascii_lowercase(),
            name: name.to_string(),
            context,
            category: category.clone(),
            short,
            extra,
            flags,
            kind,
        });
    }
    rows.sort_by(|a, b| a.key.cmp(&b.key));
    if rows.windows(2).any(|w| w[0].key == w[1].key) {
        return Err("guc_parameters.dat: two parameters have the same name".to_string());
    }

    let mut out = String::from(
        "//! The configuration parameters of PostgreSQL, one for each entry of `guc_parameters.dat` that the release build has, with the options arrays and the group texts of `guc_tables.c`.\n\
         //!\n\
         //! `cargo xtask guc` makes this file from `vendor/postgres-19/src/backend/utils/misc/guc_parameters.dat` and `guc_tables.c`. Do not edit it.\n\
         \n\
         use crate::guc::{Context, EnumOption, Kind, Parameter, flag};\n",
    );
    for (options, ()) in &used {
        let choices = &arrays[options];
        let _ = writeln!(
            out,
            "\n/// `{options}`.\n#[rustfmt::skip]\nstatic {}: [EnumOption; {}] = [",
            options.to_ascii_uppercase(),
            choices.len()
        );
        for (choice, (_, value)) in choices.iter().zip(values(choices)) {
            let _ = writeln!(
                out,
                "    EnumOption {{ name: {:?}, value: {value}, hidden: {} }},",
                choice.name, choice.hidden
            );
        }
        out.push_str("];\n");
    }
    let _ = writeln!(
        out,
        "\n/// Every parameter, in the order of the names in lower case.\n\
         #[rustfmt::skip]\n\
         pub(crate) static PARAMETERS: [Parameter; {}] = [",
        rows.len()
    );
    for Row { name, context, category, short, extra, flags, kind, .. } in &rows {
        let _ = writeln!(
            out,
            "    Parameter {{\n        \
             name: {name:?},\n        \
             context: Context::{context},\n        \
             category: {category:?},\n        \
             short_desc: {short:?},\n        \
             extra_desc: {extra},\n        \
             flags: {flags},\n        \
             kind: {kind},\n    \
             }},"
        );
    }
    out.push_str("];\n");
    Ok(out)
}

/// The value of each entry of an options array: the entries with the same C constant share a value, and the values count up from 0 in the order of the first entry of each constant.
fn values(choices: &[Choice]) -> Vec<(String, usize)> {
    let mut seen: Vec<&str> = Vec::new();
    choices
        .iter()
        .map(|choice| {
            let value = match seen.iter().position(|c| *c == choice.constant) {
                Some(value) => value,
                None => {
                    seen.push(&choice.constant);
                    seen.len() - 1
                }
            };
            (choice.constant.clone(), value)
        })
        .collect()
}

/// The lines of a C file that the oracle build compiles, with the `#if` lines removed. Only `#ifdef`, `#ifndef`, `#if` with `defined()`, `&&` and `||`, `#else` and `#endif` are known.
fn preprocess(text: &str) -> Result<String, String> {
    let mut stack: Vec<(bool, bool)> = Vec::new();
    let mut out = String::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some(directive) = trimmed.strip_prefix('#') else {
            if stack.iter().all(|(on, _)| *on) {
                out.push_str(line);
                out.push('\n');
            }
            continue;
        };
        let directive = directive.trim_start();
        let (word, rest) = directive.split_once(char::is_whitespace).unwrap_or((directive, ""));
        let rest = rest.split("/*").next().unwrap_or("").trim();
        match word {
            "ifdef" => stack.push((DEFINED.contains(&rest), false)),
            "ifndef" => stack.push((!DEFINED.contains(&rest), false)),
            "if" => {
                let on = rest.split("||").any(|all| {
                    all.split("&&").all(|atom| {
                        let atom = atom.trim();
                        let atom = atom
                            .strip_prefix("defined(")
                            .and_then(|a| a.strip_suffix(')'))
                            .unwrap_or(atom);
                        DEFINED.contains(&atom.trim())
                    })
                });
                stack.push((on, false));
            }
            "else" => match stack.last_mut() {
                Some((on, seen)) if !*seen => {
                    *on = !*on;
                    *seen = true;
                }
                _ => return Err(format!("an #else with no #if: {line}")),
            },
            "endif" => {
                stack.pop().ok_or_else(|| format!("an #endif with no #if: {line}"))?;
            }
            "include" | "define" | "undef" => {}
            _ => return Err(format!("an unknown directive: {line}")),
        }
    }
    if !stack.is_empty() {
        return Err("an #if with no #endif".to_string());
    }
    Ok(out)
}

/// The options arrays of a preprocessed C file, by the name of the array.
fn options_arrays(text: &str) -> Result<BTreeMap<String, Vec<Choice>>, String> {
    let mut arrays = BTreeMap::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let Some((_, rest)) = line.split_once("struct config_enum_entry ") else {
            continue;
        };
        let Some((name, "] = {")) = rest.split_once('[').map(|(n, r)| (n, r.trim_end())) else {
            continue;
        };
        let mut choices = Vec::new();
        loop {
            let line = lines.next().ok_or_else(|| format!("{name} has no end"))?;
            let line = line.split("/*").next().unwrap_or("").trim();
            if line == "};" {
                break;
            }
            if line.is_empty() || line.starts_with("{NULL") {
                continue;
            }
            let inner = line
                .strip_prefix('{')
                .and_then(|l| l.strip_suffix("},"))
                .ok_or_else(|| format!("{name} has a bad entry: {line}"))?;
            let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
            let (text, constant, hidden) = match parts[..] {
                [text, constant] => (text, constant, "false"),
                [text, constant, hidden] => (text, constant, hidden),
                _ => return Err(format!("{name} has a bad entry: {line}")),
            };
            choices.push(Choice {
                name: c_string(text).ok_or_else(|| format!("{name} has a bad name: {line}"))?,
                constant: constant.to_string(),
                hidden: match hidden {
                    "true" => true,
                    "false" => false,
                    _ => return Err(format!("{name} has a bad hidden mark: {line}")),
                },
            });
        }
        if arrays.insert(name.to_string(), choices).is_some() {
            return Err(format!("{name} is two times in the C files"));
        }
    }
    Ok(arrays)
}

/// The text of each group in `config_group_names` of `guc_tables.c`.
fn group_names(text: &str) -> Result<BTreeMap<String, String>, String> {
    let start =
        text.find("config_group_names[] =").ok_or("guc_tables.c has no config_group_names")?;
    let mut groups = BTreeMap::new();
    for line in text[start..].lines().skip(1) {
        let line = line.trim();
        if line == "};" {
            return Ok(groups);
        }
        let Some(rest) = line.strip_prefix('[') else {
            continue;
        };
        let (group, rest) = rest
            .split_once("] = gettext_noop(")
            .ok_or_else(|| format!("guc_tables.c: a bad group line: {line}"))?;
        let text = rest
            .strip_suffix("),")
            .and_then(c_string)
            .ok_or_else(|| format!("guc_tables.c: a bad group line: {line}"))?;
        groups.insert(group.to_string(), text);
    }
    Err("guc_tables.c: config_group_names has no end".to_string())
}

/// The value of a C string literal, or `None` if the text is not one.
fn c_string(text: &str) -> Option<String> {
    let inner = text.strip_prefix('"')?.strip_suffix('"')?;
    if inner.replace("\\\\", "").replace("\\\"", "").contains('"') {
        return None;
    }
    unescape(inner)
}

/// A text with the backslash escapes of C read, for the escapes the files use: a backslash before a backslash, a quote or a double quote.
fn unescape(text: &str) -> Option<String> {
    let mut value = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next()? {
                c @ ('\\' | '\'' | '"') => value.push(c),
                _ => return None,
            },
            c => value.push(c),
        }
    }
    Some(value)
}

/// The value of a C expression of numbers, macros, `+`, `-`, `*`, `/`, `Min()`, casts and parentheses. A `/` of two integers drops the fraction, as in C.
fn eval(text: &str, macros: &BTreeMap<&str, &str>) -> Result<f64, String> {
    let mut tokens = Vec::new();
    lex(text, macros, &mut tokens, 0)?;
    let mut at = 0;
    let (value, _) = sum(&tokens, &mut at)?;
    if at != tokens.len() {
        return Err(format!("`{text}` has more after the expression"));
    }
    Ok(value)
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Number(f64, bool),
    Op(char),
    Min,
}

fn lex(
    text: &str,
    macros: &BTreeMap<&str, &str>,
    tokens: &mut Vec<Token>,
    depth: usize,
) -> Result<(), String> {
    if depth > 8 {
        return Err(format!("`{text}` nests macros too deep"));
    }
    let text = match text.split_once("/*") {
        Some((before, after)) => {
            let (_, after) = after.split_once("*/").ok_or("a comment with no end")?;
            format!("{before} {after}")
        }
        None => text.to_string(),
    };
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() || c == '.' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let float = word.contains(['.', 'e']);
            // A C integer with a leading 0 is octal, as the file modes are.
            let value = match word.strip_prefix('0') {
                Some(octal) if !float && !octal.is_empty() => i64::from_str_radix(octal, 8)
                    .map(|v| v as f64)
                    .map_err(|_| format!("`{word}` is not a number"))?,
                _ => word.parse::<f64>().map_err(|_| format!("`{word}` is not a number"))?,
            };
            tokens.push(Token::Number(value, float));
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            if word == "Min" {
                tokens.push(Token::Min);
            } else if matches!(word.as_str(), "int" | "size_t" | "Size") {
                // A cast: drop the `(` before it and the `)` after it.
                if tokens.pop() != Some(Token::Op('(')) || chars.get(i) != Some(&')') {
                    return Err(format!("`{word}` is not in a cast"));
                }
                i += 1;
            } else {
                let value = macros.get(word.as_str()).ok_or_else(|| format!("no macro {word}"))?;
                tokens.push(Token::Op('('));
                lex(value, macros, tokens, depth + 1)?;
                tokens.push(Token::Op(')'));
            }
        } else if "+-*/(),".contains(c) {
            tokens.push(Token::Op(c));
            i += 1;
        } else {
            return Err(format!("`{c}` is not known in an expression"));
        }
    }
    Ok(())
}

fn sum(tokens: &[Token], at: &mut usize) -> Result<(f64, bool), String> {
    let (mut value, mut float) = product(tokens, at)?;
    while let Some(Token::Op(op @ ('+' | '-'))) = tokens.get(*at) {
        *at += 1;
        let (right, right_float) = product(tokens, at)?;
        value = if *op == '+' { value + right } else { value - right };
        float |= right_float;
    }
    Ok((value, float))
}

fn product(tokens: &[Token], at: &mut usize) -> Result<(f64, bool), String> {
    let (mut value, mut float) = unary(tokens, at)?;
    while let Some(Token::Op(op @ ('*' | '/'))) = tokens.get(*at) {
        *at += 1;
        let (right, right_float) = unary(tokens, at)?;
        float |= right_float;
        value = match op {
            '*' => value * right,
            _ if float => value / right,
            _ => (value / right).trunc(),
        };
    }
    Ok((value, float))
}

fn unary(tokens: &[Token], at: &mut usize) -> Result<(f64, bool), String> {
    let token = tokens.get(*at).cloned().ok_or("an expression ends too early")?;
    *at += 1;
    let expect = |at: &mut usize, op: char| {
        if tokens.get(*at) == Some(&Token::Op(op)) {
            *at += 1;
            Ok(())
        } else {
            Err(format!("a `{op}` is missing"))
        }
    };
    match token {
        Token::Number(value, float) => Ok((value, float)),
        Token::Op('-') => unary(tokens, at).map(|(value, float)| (-value, float)),
        Token::Op('(') => {
            let value = sum(tokens, at)?;
            expect(at, ')')?;
            Ok(value)
        }
        Token::Min => {
            expect(at, '(')?;
            let (a, a_float) = sum(tokens, at)?;
            expect(at, ',')?;
            let (b, b_float) = sum(tokens, at)?;
            expect(at, ')')?;
            Ok((a.min(b), a_float || b_float))
        }
        Token::Op(op) => Err(format!("`{op}` is not where a value can be")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_expressions_follow_c() {
        let macros: BTreeMap<&str, &str> = MACROS.into_iter().collect();
        let eval = |text| eval(text, &macros).unwrap();
        assert_eq!(eval("(512 * 1024) / BLCKSZ"), 64.0);
        assert_eq!(eval("INT_MAX / 1000"), 2_147_483.0);
        assert_eq!(eval("(int) Min((size_t) INT_MAX, SIZE_MAX / (1024 * 1024))"), 2_147_483_647.0);
        assert_eq!(eval("10 * HOURS_PER_DAY * MINS_PER_HOUR /* 10 days */"), 14_400.0);
        assert_eq!(eval("DEFAULT_IO_COMBINE_LIMIT"), 16.0);
        assert_eq!(eval("INT_MIN"), -2_147_483_648.0);
        assert_eq!(eval("-1"), -1.0);
        assert_eq!(eval("1e10"), 1e10);
        assert_eq!(eval("0777"), 511.0);
        assert_eq!(eval("0"), 0.0);
    }

    #[test]
    fn the_preprocessor_keeps_the_lines_of_the_oracle_build() {
        let text = "a\n#ifdef USE_LZ4\nb\n#else\nc\n#endif\n#if defined(HAVE_COPYFILE) && defined(COPYFILE_CLONE_FORCE) || defined(HAVE_COPY_FILE_RANGE)\nd\n#endif\n#if defined(HAVE_COPYFILE) || defined(X)\ne\n#endif\n";
        assert_eq!(preprocess(text).unwrap(), "a\nc\nd\n");
        assert!(preprocess("#ifdef A\n").is_err());
    }

    #[test]
    fn an_options_array_gives_shared_values_to_aliases() {
        let arrays = options_arrays(&preprocess(OTHER_ARRAYS).unwrap()).unwrap();
        let levels = &arrays["wal_level_options"];
        let names: Vec<_> = levels.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["minimal", "replica", "archive", "hot_standby", "logical"]);
        let numbers: Vec<_> = values(levels).into_iter().map(|(_, v)| v).collect();
        assert_eq!(numbers, [0, 1, 1, 1, 2]);
        assert_eq!(arrays["io_method_options"].len(), 2);
    }
}
