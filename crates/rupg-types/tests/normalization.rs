//! The conformance test of Unicode Standard Annex 15 on every line of `vendor/unicode/NormalizationTest.txt`.

// The test reads a vendored file of the repository, not a database file.
#![allow(clippy::disallowed_methods)]

use rupg_types::unicode::{NormalForm, normalize};

fn codes(field: &str) -> Vec<u32> {
    field.split(' ').map(|h| u32::from_str_radix(h, 16).unwrap()).collect()
}

#[test]
fn every_line_of_the_test_file() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../vendor/unicode/NormalizationTest.txt");
    let text = std::fs::read_to_string(path).unwrap();
    let mut lines = 0;
    let mut listed = std::collections::BTreeSet::new();
    let mut part1 = false;
    for line in text.lines() {
        if line.starts_with("@Part") {
            part1 = line.starts_with("@Part1");
            continue;
        }
        let line = line.split('#').next().unwrap();
        if line.trim().is_empty() {
            continue;
        }
        let c: Vec<Vec<u32>> = line.split(';').take(5).map(codes).collect();
        let (nfc, nfd, nfkc, nfkd) = (&c[1], &c[2], &c[3], &c[4]);
        // The conformance rules of the file: c2 == NFC(c1) == NFC(c2) == NFC(c3), c4 == NFC(c4) == NFC(c5), and the same for the other forms.
        for x in &c[..3] {
            assert_eq!(&normalize(NormalForm::Nfc, x), nfc, "NFC of {x:X?}");
            assert_eq!(&normalize(NormalForm::Nfd, x), nfd, "NFD of {x:X?}");
        }
        for x in &c {
            assert_eq!(&normalize(NormalForm::Nfkc, x), nfkc, "NFKC of {x:X?}");
            assert_eq!(&normalize(NormalForm::Nfkd, x), nfkd, "NFKD of {x:X?}");
        }
        for x in &c[3..] {
            assert_eq!(&normalize(NormalForm::Nfc, x), nfkc, "NFC of {x:X?}");
            assert_eq!(&normalize(NormalForm::Nfd, x), nfkd, "NFD of {x:X?}");
        }
        if part1 {
            listed.insert(c[0][0]);
        }
        lines += 1;
    }
    assert!(lines > 19_000, "only {lines} lines");
    // Each scalar value that part 1 does not list is unchanged by all four forms.
    for c in (0..0x11_0000u32).filter(|c| char::from_u32(*c).is_some() && !listed.contains(c)) {
        for form in [NormalForm::Nfc, NormalForm::Nfd, NormalForm::Nfkc, NormalForm::Nfkd] {
            assert_eq!(normalize(form, &[c]), [c], "{form:?} of {c:X}");
        }
    }
}
