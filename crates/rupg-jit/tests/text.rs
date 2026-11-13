// Lifted from rudb crates/rudb-qc-ir/tests/text.rs at cd9f9676 (2026-10-05).
// Changes: the modules are included at build time, as the tests of rupg read no files.

//! The text form round-trips, and the verifier accepts the modules it should and rejects the ones
//! under `tests/illegal/` for the rule each file is named after.

use rupg_jit::ir::{parse, print, verify};

/// The modules of `tests/text/`, by name.
const TEXT: [(&str, &str); 2] =
    [("forms", include_str!("text/forms.qir")), ("q1a", include_str!("text/q1a.qir"))];

/// The modules of `tests/illegal/`, each named after the rule it breaks.
const ILLEGAL: [(&str, &str); 4] = [
    ("v10", include_str!("illegal/v10.qir")),
    ("v11", include_str!("illegal/v11.qir")),
    ("v12", include_str!("illegal/v12.qir")),
    ("v9", include_str!("illegal/v9.qir")),
];

#[test]
fn legal_modules_verify_and_round_trip() {
    for (name, text) in TEXT {
        let m = parse(text).unwrap_or_else(|e| panic!("{name}: {e}"));
        if let Err(errors) = verify(&m) {
            let list: Vec<String> = errors.iter().map(ToString::to_string).collect();
            panic!("{name} does not verify:\n{}", list.join("\n"));
        }
        let once = print(&m);
        let again = parse(&once).unwrap_or_else(|e| panic!("{name} does not reparse: {e}\n{once}"));
        assert_eq!(print(&again), once, "{name} does not round-trip");
        assert_eq!(again.funcs[0].count(), m.funcs[0].count());
    }
}

#[test]
fn illegal_modules_are_rejected_for_their_rule() {
    let all = ILLEGAL;
    assert!(all.len() >= 4, "V9 to V12 each need a module");
    for (name, text) in all {
        let m = parse(text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let rule = name.to_uppercase();
        let errors = verify(&m).expect_err(&format!("{name} verifies and should not"));
        assert!(
            errors.iter().all(|e| e.rule == rule),
            "{name} breaks other rules too: {:?}",
            errors.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
    }
}

#[test]
fn parse_errors_name_the_line() {
    let e = parse(
        "module x\n\nfunc @f version=v plan=#0\nblock b0(ptr %st, ptr %m):\n  %x = frob i64 1\n",
    )
    .unwrap_err();
    assert_eq!(e.line, 5);
    let e = parse("module x\nfunc @f version=v plan=#0\nblock b0(ptr %st, ptr %m):\n  ret %nope\n")
        .unwrap_err();
    assert!(e.message.contains("never defined"), "{e}");
}
