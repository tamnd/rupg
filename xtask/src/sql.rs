//! The parser of `rupg-sql`, made from the vendored PostgreSQL files.
//!
//! `cargo xtask sql` writes the generated files of `crates/rupg-sql/src/generated` from the files in `vendor/postgres-19` and from the ported actions in `crates/rupg-sql/src/actions`. `cargo xtask sql --check` writes nothing and fails if a generated file is not what its generator writes today. CI runs the check. `cargo xtask sql --bison` runs bison on the grammar without its C and compares its tables with the generated tables, entry by entry.
//!
//! `gram.y` gives the parse tables through `sql/gram.rs`, which removes the C, and `sql/lalr.rs`, which makes the LALR(1) tables as bison 2.3 makes them, with the same state numbers. `kwlist.h` gives the keywords. `nodes.h`, `lockoptions.h`, `primnodes.h`, `parsenodes.h`, `value.h` and the other headers give the node types of the raw parse tree through `sql/nodes.rs`. `sql/glue.rs` and `sql/translate.rs` translate the C actions of `gram.y` to Rust. See `spec/07-sql-types-and-catalog.md` section 7.2.
//!
//! Lifted from `xtask/src/postgres.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

mod glue;
mod gram;
mod lalr;
mod nodes;
mod pgparse;
mod translate;

use std::path::Path;

use crate::read;

const VENDOR: &str = "vendor/postgres-19/src";
const HEADERS: [&str; 16] = [
    "include/nodes/nodes.h",
    "include/nodes/lockoptions.h",
    "include/nodes/primnodes.h",
    "include/nodes/parsenodes.h",
    "include/nodes/value.h",
    "include/catalog/pg_class.h",
    "include/catalog/pg_am.h",
    "include/catalog/pg_attribute.h",
    "include/catalog/pg_trigger.h",
    "include/catalog/index.h",
    "include/commands/trigger.h",
    "include/utils/datetime.h",
    "include/utils/timestamp.h",
    "include/utils/xml.h",
    "include/storage/lockdefs.h",
    "include/common/relpath.h",
];
const GRAM: &str = "backend/parser/gram.y";
const KWLIST: &str = "include/parser/kwlist.h";
const ERRCODES: &str = "backend/utils/errcodes.txt";
const PG_TYPE: &str = "include/catalog/pg_type.dat";
const ACTIONS: &str = "crates/rupg-sql/src/actions/";
const OUT: &str = "crates/rupg-sql/src/generated";

/// One generated file: its name in `OUT`, its inputs, and the generator, which gets the text of the inputs in the same order. An input that ends with `/` is relative to the workspace root, and the others are relative to `VENDOR`.
struct Generated {
    output: &'static str,
    inputs: Vec<&'static str>,
    generate: fn(&[String]) -> Result<String, String>,
}

fn generated() -> Vec<Generated> {
    let mut model: Vec<&'static str> = HEADERS.to_vec();
    model.extend([ERRCODES, PG_TYPE, GRAM]);
    let mut with_actions = model.clone();
    with_actions.push(ACTIONS);
    vec![
        Generated { output: "gram.rules", inputs: vec![GRAM], generate: |t| gram::rules(&t[0]) },
        Generated {
            output: "productions.txt",
            inputs: vec![GRAM],
            generate: |t| gram::productions(&t[0]),
        },
        Generated { output: "tables.rs", inputs: vec![GRAM], generate: pgparse::tables },
        Generated {
            output: "keywords.rs",
            inputs: vec![KWLIST, GRAM],
            generate: pgparse::keywords,
        },
        Generated { output: "nodes.rs", inputs: model, generate: nodes::nodes },
        Generated { output: "glue.rs", inputs: with_actions, generate: glue::glue },
    ]
}

/// The text of the inputs of a generated file. The text of a directory is the text of its `.rs` files, in the order of their names.
fn inputs(root: &Path, generated: &Generated) -> Result<Vec<String>, String> {
    generated
        .inputs
        .iter()
        .map(|input| {
            if !input.ends_with('/') {
                return read(&root.join(VENDOR).join(input));
            }
            let dir = root.join(input);
            let entries =
                std::fs::read_dir(&dir).map_err(|e| format!("could not list {input}: {e}"))?;
            let mut paths: Vec<_> = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|e| e == "rs"))
                .collect();
            paths.sort();
            let texts: Vec<String> =
                paths.iter().map(|path| read(path)).collect::<Result<_, _>>()?;
            Ok(texts.concat())
        })
        .collect()
}

pub(crate) fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let mut check = false;
    for arg in args {
        match arg.as_str() {
            "--check" => check = true,
            "--bison" => {
                return pgparse::compare_with_bison(
                    root,
                    &root.join(VENDOR).join(GRAM),
                    &root.join(OUT).join("gram.rules"),
                );
            }
            other => return Err(format!("sql: unknown argument {other:?}")),
        }
    }
    let mut stale = Vec::new();
    for generated in generated() {
        let text = (generated.generate)(&inputs(root, &generated)?)?;
        let path = root.join(OUT).join(generated.output);
        if std::fs::read_to_string(&path).ok().map(|t| t.replace("\r\n", "\n")).as_deref()
            == Some(text.as_str())
        {
            continue;
        }
        if check {
            stale.push(generated.output);
        } else {
            std::fs::write(&path, text)
                .map_err(|e| format!("could not write {}: {e}", path.display()))?;
            println!("sql: wrote {OUT}/{}", generated.output);
        }
    }
    if stale.is_empty() {
        println!("sql: the generated files match the vendored files and the ported actions");
        Ok(())
    } else {
        Err(format!(
            "sql: run `cargo xtask sql` and commit the result, these files in {OUT} are not up to date: {}",
            stale.join(", ")
        ))
    }
}
