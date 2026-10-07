//! The actions of `SELECT` that the translator cannot write: the select options of `select_no_parens`, the joins, and the column options of `XMLTABLE`.
//!
//! Lifted from `crates/rudb-pgparse/src/actions/select.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use super::*;
use crate::error::Error;
use crate::generated::glue::{SelectLimit, rules};
use crate::nodes::*;

impl rules::select_no_parens for Parser<'_> {
    fn select_no_parens_2(&mut self, v1: Option<Node>, v2: List) -> Result<Option<Node>, Error> {
        let mut v1 = v1;
        insertSelectOptions(castMut(&mut v1)?, v2, List::new(), None, None, self)?;
        Ok(v1)
    }

    fn select_no_parens_3(
        &mut self,
        v1: Option<Node>,
        v2: List,
        v3: List,
        v4: Option<Box<SelectLimit>>,
    ) -> Result<Option<Node>, Error> {
        let mut v1 = v1;
        insertSelectOptions(castMut(&mut v1)?, v2, v3, v4, None, self)?;
        Ok(v1)
    }

    fn select_no_parens_4(
        &mut self,
        v1: Option<Node>,
        v2: List,
        v3: Option<Box<SelectLimit>>,
        v4: List,
    ) -> Result<Option<Node>, Error> {
        let mut v1 = v1;
        insertSelectOptions(castMut(&mut v1)?, v2, v4, v3, None, self)?;
        Ok(v1)
    }

    fn select_no_parens_5(
        &mut self,
        v1: Option<Box<WithClause>>,
        v2: Option<Node>,
    ) -> Result<Option<Node>, Error> {
        let mut v2 = v2;
        insertSelectOptions(castMut(&mut v2)?, List::new(), List::new(), None, v1, self)?;
        Ok(v2)
    }

    fn select_no_parens_6(
        &mut self,
        v1: Option<Box<WithClause>>,
        v2: Option<Node>,
        v3: List,
    ) -> Result<Option<Node>, Error> {
        let mut v2 = v2;
        insertSelectOptions(castMut(&mut v2)?, v3, List::new(), None, v1, self)?;
        Ok(v2)
    }

    fn select_no_parens_7(
        &mut self,
        v1: Option<Box<WithClause>>,
        v2: Option<Node>,
        v3: List,
        v4: List,
        v5: Option<Box<SelectLimit>>,
    ) -> Result<Option<Node>, Error> {
        let mut v2 = v2;
        insertSelectOptions(castMut(&mut v2)?, v3, v4, v5, v1, self)?;
        Ok(v2)
    }

    fn select_no_parens_8(
        &mut self,
        v1: Option<Box<WithClause>>,
        v2: Option<Node>,
        v3: List,
        v4: Option<Box<SelectLimit>>,
        v5: List,
    ) -> Result<Option<Node>, Error> {
        let mut v2 = v2;
        insertSelectOptions(castMut(&mut v2)?, v3, v5, v4, v1, self)?;
        Ok(v2)
    }
}

/// A `JoinExpr` with an `ON` or a `USING` clause. A `USING` clause is a list of the column names and the alias of the join.
fn join(
    jointype: JoinType,
    larg: Option<Node>,
    rarg: Option<Node>,
    join_qual: Option<Node>,
) -> Result<Option<Box<JoinExpr>>, Error> {
    let mut n = JoinExpr { jointype, isNatural: false, larg, rarg, ..JoinExpr::default() };
    match join_qual {
        Some(Node::List(qual)) => {
            let [usingClause, alias] = elements(qual);
            n.usingClause = castList(usingClause)?;
            n.join_using_alias = castNode(alias)?;
        }
        quals => n.quals = quals,
    }
    Ok(Some(Box::new(n)))
}

impl rules::joined_table for Parser<'_> {
    fn joined_table_3(
        &mut self,
        v1: Option<Node>,
        v2: JoinType,
        v4: Option<Node>,
        v5: Option<Node>,
    ) -> Result<Option<Box<JoinExpr>>, Error> {
        join(v2, v1, v4, v5)
    }

    fn joined_table_4(
        &mut self,
        v1: Option<Node>,
        v3: Option<Node>,
        v4: Option<Node>,
    ) -> Result<Option<Box<JoinExpr>>, Error> {
        // A `join_type` that can be empty does not work in the grammar.
        join(JoinType::JOIN_INNER, v1, v3, v4)
    }
}

impl rules::xmltable_column_el for Parser<'_> {
    fn xmltable_column_el_2(
        &mut self,
        v1: Option<Str>,
        v2: Option<Box<TypeName>>,
        v3: List,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        let mut fc = RangeTableFuncCol {
            colname: v1,
            typeName: v2,
            for_ordinality: false,
            is_not_null: false,
            colexpr: None,
            coldefexpr: None,
            location: at1,
        };
        let mut nullability_seen = false;
        for option in v3 {
            let defel = castNode::<DefElem>(option)?
                .ok_or_else(|| Error::internal("a NULL column option"))?;
            let location = defel.location;
            let error = |message: &str| Err(self.error(ERRCODE_SYNTAX_ERROR, message, location));
            match defel.defname.as_deref() {
                Some("default") => {
                    if fc.coldefexpr.is_some() {
                        return error("only one DEFAULT value is allowed");
                    }
                    fc.coldefexpr = defel.arg;
                }
                Some("path") => {
                    if fc.colexpr.is_some() {
                        return error("only one PATH value per column is allowed");
                    }
                    fc.colexpr = defel.arg;
                }
                Some("__pg__is_not_null") => {
                    if nullability_seen {
                        let colname = fc.colname.as_deref().unwrap_or_default();
                        return error(&format!(
                            "conflicting or redundant NULL / NOT NULL declarations for column \"{colname}\""
                        ));
                    }
                    fc.is_not_null = boolVal(defel.arg.as_ref())?;
                    nullability_seen = true;
                }
                name => {
                    let name = name.unwrap_or_default();
                    return error(&format!("unrecognized column option \"{name}\""));
                }
            }
        }
        Ok(Some(fc.into()))
    }
}
