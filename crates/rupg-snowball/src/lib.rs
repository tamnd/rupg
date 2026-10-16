//! The Snowball stemmers of PostgreSQL 19.
//!
//! The module `dict_snowball` of PostgreSQL has 33 stemmers from Snowball at the commit `2d2e312df56f2ede014a4ffb3e91e6dea43c24be`. This crate has the Rust code that Snowball generates for the same 33 algorithms at the same commit, and the Rust runtime of Snowball. The text search dictionaries of the template `snowball` use it. The code is under the BSD 3-clause license of Snowball. See `LICENSE` and `README.md`.

#![forbid(unsafe_code)]

use std::fmt;

#[allow(
    warnings,
    clippy::all,
    clippy::pedantic,
    clippy::missing_panics_doc,
    clippy::todo,
    clippy::dbg_macro
)]
mod algorithms;
#[allow(warnings, clippy::all, clippy::pedantic, clippy::missing_panics_doc)]
mod among;
#[allow(warnings, clippy::all, clippy::pedantic, clippy::missing_panics_doc)]
mod env;

use algorithms as a;
use env::SnowballEnv;

/// One Snowball stemmer.
#[derive(Clone, Copy)]
pub struct Stemmer {
    name: &'static str,
    run: fn(&mut SnowballEnv<'_>) -> bool,
}

/// The stemmers in the order of the table `stemmer_modules` of `dict_snowball.c`.
const STEMMERS: [Stemmer; 33] = [
    Stemmer { name: "arabic", run: a::arabic_stemmer::stem },
    Stemmer { name: "armenian", run: a::armenian_stemmer::stem },
    Stemmer { name: "basque", run: a::basque_stemmer::stem },
    Stemmer { name: "catalan", run: a::catalan_stemmer::stem },
    Stemmer { name: "danish", run: a::danish_stemmer::stem },
    Stemmer { name: "dutch", run: a::dutch_stemmer::stem },
    Stemmer { name: "dutch_porter", run: a::dutch_porter_stemmer::stem },
    Stemmer { name: "english", run: a::english_stemmer::stem },
    Stemmer { name: "esperanto", run: a::esperanto_stemmer::stem },
    Stemmer { name: "estonian", run: a::estonian_stemmer::stem },
    Stemmer { name: "finnish", run: a::finnish_stemmer::stem },
    Stemmer { name: "french", run: a::french_stemmer::stem },
    Stemmer { name: "german", run: a::german_stemmer::stem },
    Stemmer { name: "greek", run: a::greek_stemmer::stem },
    Stemmer { name: "hindi", run: a::hindi_stemmer::stem },
    Stemmer { name: "hungarian", run: a::hungarian_stemmer::stem },
    Stemmer { name: "indonesian", run: a::indonesian_stemmer::stem },
    Stemmer { name: "irish", run: a::irish_stemmer::stem },
    Stemmer { name: "italian", run: a::italian_stemmer::stem },
    Stemmer { name: "lithuanian", run: a::lithuanian_stemmer::stem },
    Stemmer { name: "nepali", run: a::nepali_stemmer::stem },
    Stemmer { name: "norwegian", run: a::norwegian_stemmer::stem },
    Stemmer { name: "porter", run: a::porter_stemmer::stem },
    Stemmer { name: "polish", run: a::polish_stemmer::stem },
    Stemmer { name: "portuguese", run: a::portuguese_stemmer::stem },
    Stemmer { name: "romanian", run: a::romanian_stemmer::stem },
    Stemmer { name: "russian", run: a::russian_stemmer::stem },
    Stemmer { name: "serbian", run: a::serbian_stemmer::stem },
    Stemmer { name: "spanish", run: a::spanish_stemmer::stem },
    Stemmer { name: "swedish", run: a::swedish_stemmer::stem },
    Stemmer { name: "tamil", run: a::tamil_stemmer::stem },
    Stemmer { name: "turkish", run: a::turkish_stemmer::stem },
    Stemmer { name: "yiddish", run: a::yiddish_stemmer::stem },
];

impl Stemmer {
    /// The stemmer for `language`. The match ignores the case of ASCII letters, as `pg_strcasecmp` does in `locate_stem_module`.
    #[must_use]
    pub fn find(language: &str) -> Option<Stemmer> {
        STEMMERS.iter().copied().find(|s| s.name.eq_ignore_ascii_case(language))
    }

    /// All the stemmers, in the order of `dict_snowball.c`.
    #[must_use]
    pub fn all() -> &'static [Stemmer] {
        &STEMMERS
    }

    /// The name of the language, for example `english`.
    #[must_use]
    pub fn name(self) -> &'static str {
        self.name
    }

    /// The stem of `word`. The caller gives a word in lowercase, as `dsnowball_lexize` does. If the stemmer gives an empty string, the result is the word, because `dsnowball_lexize` keeps its input when the length of the output is zero.
    #[must_use]
    pub fn stem(self, word: &str) -> String {
        let mut env = SnowballEnv::create(word);
        (self.run)(&mut env);
        let out = env.get_current();
        if out.is_empty() { word.to_string() } else { out.into_owned() }
    }
}

impl fmt::Debug for Stemmer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Stemmer").field(&self.name).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::Stemmer;

    #[test]
    fn find_ignores_ascii_case() {
        assert_eq!(Stemmer::find("English").map(Stemmer::name), Some("english"));
        assert_eq!(Stemmer::find("DUTCH_PORTER").map(Stemmer::name), Some("dutch_porter"));
        assert!(Stemmer::find("klingon").is_none());
        assert_eq!(Stemmer::all().len(), 33);
    }

    #[test]
    fn english() {
        let s = Stemmer::find("english").expect("english");
        for (word, stem) in [
            ("foxes", "fox"),
            ("jumped", "jump"),
            ("quick", "quick"),
            ("running", "run"),
            ("generously", "generous"),
        ] {
            assert_eq!(s.stem(word), stem, "{word}");
        }
    }

    /// `tests/oracle.tsv` has 120 words for each of the 33 languages, from the file `voc.txt` of the Snowball test data, with the stem that PostgreSQL 19 gives. Each row is the language, the word and the stem.
    #[test]
    fn oracle() {
        let mut bad = Vec::new();
        for line in include_str!("../tests/oracle.tsv").lines() {
            let mut parts = line.split('\t');
            let (Some(lang), Some(word), Some(stem)) = (parts.next(), parts.next(), parts.next())
            else {
                panic!("bad line {line:?}");
            };
            let got = Stemmer::find(lang).expect("language").stem(word);
            if got != stem {
                bad.push(format!("{lang} {word}: {got} and not {stem}"));
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }
}
