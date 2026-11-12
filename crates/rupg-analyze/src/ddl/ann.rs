//! `hnsw` and `ivfflat`, the index access methods of pgvector: their options, as `hnswoptions` and `ivfflatoptions` give them, and the checks of `InitBuildState` of `hnswbuild.c` and `ivfbuild.c`, which read the type, the type modifier and the operator class of the column and the options of the index before they read a row.

use rupg_catalog::NewIndexColumn;
use rupg_common::{Error, Result, SqlState};
use rupg_types::oid::VARBIT;

use super::Definer;
use super::reloptions::{self, Kind, Opt};

/// An index access method of pgvector, which the symbol of its handler names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Ann {
    /// `hnswhandler`.
    Hnsw,
    /// `ivfflathandler`.
    Ivfflat,
}

/// `hnsw_relopt_kind`: `m` between `HNSW_MIN_M` and `HNSW_MAX_M`, and `ef_construction` between `HNSW_MIN_EF_CONSTRUCTION` and `HNSW_MAX_EF_CONSTRUCTION`.
pub(super) const HNSW: &[Opt] = &[
    Opt { name: "m", kind: Kind::Int(2, 100), toast: false },
    Opt { name: "ef_construction", kind: Kind::Int(4, 1000), toast: false },
];

/// `ivfflat_relopt_kind`: `lists` between `IVFFLAT_MIN_LISTS` and `IVFFLAT_MAX_LISTS`.
pub(super) const IVFFLAT: &[Opt] =
    &[Opt { name: "lists", kind: Kind::Int(1, 32768), toast: false }];

/// `HNSW_DEFAULT_M`.
const DEFAULT_M: i32 = 16;
/// `HNSW_DEFAULT_EF_CONSTRUCTION`.
const DEFAULT_EF_CONSTRUCTION: i32 = 64;

impl Ann {
    /// The name in the errors of the build.
    fn name(self) -> &'static str {
        match self {
            Self::Hnsw => "hnsw",
            Self::Ivfflat => "ivfflat",
        }
    }

    /// The options of an index of the method.
    pub(super) fn options(self) -> &'static [Opt] {
        match self {
            Self::Hnsw => HNSW,
            Self::Ivfflat => IVFFLAT,
        }
    }

    /// `HNSW_TYPE_INFO_PROC` and `IVFFLAT_TYPE_INFO_PROC`: the number of the support function that gives the facts of the type.
    fn type_info_proc(self) -> i16 {
        match self {
            Self::Hnsw => 3,
            Self::Ivfflat => 5,
        }
    }

    /// `maxDimensions` of the facts of the type that the support function of the symbol `symbol` gives, as `HnswGetTypeInfo` and `IvfflatGetTypeInfo` give them: the type `vector` when the class has no such function, else `halfvec`, `bit` or `sparsevec`.
    fn max_dimensions(self, symbol: Option<&str>) -> i32 {
        match (self, symbol) {
            (_, Some("hnsw_halfvec_support" | "ivfflat_halfvec_support")) => 4000,
            (_, Some("hnsw_bit_support" | "ivfflat_bit_support")) => 64000,
            // `SPARSEVEC_MAX_DIM`.
            (Self::Hnsw, Some("hnsw_sparsevec_support")) => 1_000_000_000,
            _ => 2000,
        }
    }
}

impl Definer<'_, '_> {
    /// The method of pgvector that the access method `method` is, by the symbol of its handler, or `None` for any other method.
    pub(super) fn ann_method(&self, method: u32) -> Option<Ann> {
        let am = self.catalog.access_method(method)?;
        match self.catalog.function(am.handler)?.src.as_str() {
            "hnswhandler" => Some(Ann::Hnsw),
            "ivfflathandler" => Some(Ann::Ivfflat),
            _ => None,
        }
    }

    /// The symbol of the support function `number` of the operator class `class` for its input type, or `None` when the family has no such function.
    fn support_symbol(&self, class: u32, number: i16) -> Option<&str> {
        let class = self.catalog.opclass(class)?;
        let proc = self.catalog.amprocs().find(|p| {
            p.family == class.family
                && p.number == number
                && p.left == class.intype
                && p.right == class.intype
        })?;
        Some(self.catalog.function(proc.function)?.src.as_str())
    }

    /// `InitBuildState` of `hnswbuild.c` and `ivfbuild.c` for the column `column` of a new index of `method` with the options `options`: the type is not `varbit`, the column has dimensions and not more than the type allows, `ef_construction` is at least twice `m`, and spherical k-means has more than one dimension.
    pub(super) fn ann_build_check(
        &self,
        method: Ann,
        column: &NewIndexColumn,
        options: &[String],
    ) -> Result<()> {
        let name = method.name();
        if column.ty == VARBIT {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("type not supported for {name} index"),
            ));
        }
        let dimensions = column.typmod;
        if dimensions < 0 {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                "column does not have dimensions",
            ));
        }
        let max = method.max_dimensions(self.support_symbol(column.class, method.type_info_proc()));
        if dimensions > max {
            return Err(Error::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!("column cannot have more than {max} dimensions for {name} index"),
            ));
        }
        match method {
            Ann::Hnsw => {
                let m = reloptions::int_option(options, "m").unwrap_or(DEFAULT_M);
                let ef = reloptions::int_option(options, "ef_construction")
                    .unwrap_or(DEFAULT_EF_CONSTRUCTION);
                if ef / 2 < m {
                    return Err(Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "ef_construction must be greater than or equal to 2 * m",
                    ));
                }
            }
            // `IVFFLAT_KMEANS_NORM_PROC`.
            Ann::Ivfflat => {
                if dimensions == 1 && self.support_symbol(column.class, 4).is_some() {
                    return Err(Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "dimensions must be greater than one for this opclass",
                    ));
                }
            }
        }
        Ok(())
    }
}
