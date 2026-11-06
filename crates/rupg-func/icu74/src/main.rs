//! Exports the collation data of ICU 74 for ICU4X: the collation markers and the locale fallback markers, from CLDR 44.1 and the ICU export data of ICU 74.1, with the Han characters of the root in the radical-stroke order of Unihan, as ICU4C builds its root. The normalizer data of ICU 74 is not in a form that the exporter of ICU4X 2.4 reads, so rupg takes NFD from the compiled data of ICU4X.

use std::fs::File;
use std::path::Path;

use icu_provider_export::blob_exporter::BlobExporter;
use icu_provider_export::prelude::*;
use icu_provider_source::{CollationRootHan, SourceDataProvider};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, cldr, icuexport, out] = &args[..] else {
        panic!("usage: icu74 CLDR ICUEXPORT OUT");
    };
    let provider = SourceDataProvider::new_custom()
        .with_cldr(Path::new(cldr))
        .expect("the CLDR 44.1 JSON")
        .with_icuexport(Path::new(icuexport))
        .expect("the ICU 74.1 export data")
        .with_collation_root_han(CollationRootHan::Unihan);
    let mut markers: Vec<DataMarkerInfo> = icu_collator::provider::MARKERS.to_vec();
    markers.extend_from_slice(icu_locale_fallback::provider::MARKERS);
    let fallbacker = LocaleFallbacker::try_new_unstable(&provider).expect("the fallback data");
    ExportDriver::new([DataLocaleFamily::FULL], DeduplicationStrategy::None.into(), fallbacker)
        .with_markers(markers)
        .export(&provider, BlobExporter::new_with_sink(Box::new(File::create(out).expect("the output file"))))
        .expect("the export");
}
