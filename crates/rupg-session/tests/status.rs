//! The report of the status of each parameter: honored, accepted or refused (spec/06 section 6.13.2).
//!
//! `status.tsv` has one row for each parameter of the table, with its name and its status. It is the report that `rupg-compat` publishes, so a change of a status must change the file in the same change. `cut -f2 status.tsv | sort | uniq -c` gives the three counts.

use rupg_session::guc::{self, HONORED, Kind, REFUSED, Status};

#[test]
fn the_report_is_the_table() {
    let report: String = guc::all()
        .iter()
        .map(|parameter| format!("{}\t{}\n", parameter.name, guc::status(parameter).name()))
        .collect();
    let recorded = include_str!("status.tsv");
    for (line, (got, want)) in report.lines().zip(recorded.lines()).enumerate() {
        assert_eq!(got, want, "line {} of status.tsv", line + 1);
    }
    assert_eq!(report.lines().count(), recorded.lines().count());
}

#[test]
fn the_lists_name_parameters() {
    let names =
        HONORED.iter().map(|(name, _)| *name).chain(REFUSED.iter().map(|(name, _, _)| *name));
    for name in names {
        let parameter = guc::find(name).unwrap_or_else(|| panic!("{name} is not a parameter"));
        assert_eq!(parameter.name, name, "the canonical name");
    }
    let sorted =
        |list: Vec<&str>| list.windows(2).all(|w| w[0].to_lowercase() < w[1].to_lowercase());
    assert!(sorted(HONORED.iter().map(|(name, _)| *name).collect()));
    assert!(sorted(REFUSED.iter().map(|(name, _, _)| *name).collect()));
}

#[test]
fn a_refused_value_is_a_value_and_not_the_default() {
    for (name, values, _) in REFUSED {
        let parameter = guc::find(name).unwrap();
        assert_eq!(guc::status(parameter), Status::Refused);
        let boot = parameter.show(&parameter.boot());
        for value in values {
            assert_ne!(*value, boot, "{name}");
            let valid = match &parameter.kind {
                Kind::Enum { options, .. } => options.iter().any(|o| o.name == *value),
                Kind::Bool { .. } => matches!(*value, "on" | "off"),
                _ => false,
            };
            assert!(valid, "{name} has no value {value}");
        }
    }
}
