//! The actions of the statements that the translator cannot write: the statement lists, `parse_toplevel` and `stmtmulti`, `CHECKPOINT`, the hash partition bound and the multiple-column `SET` of `UPDATE`.
//!
//! Lifted from `crates/rudb-pgparse/src/actions/stmt.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use super::{Parser, castMut, castRef, defGetInt32, list_length, makeRawStmt, updateRawStmtEnd};
use crate::error::Error;
use crate::generated::glue::rules;
use crate::nodes::*;

impl rules::parse_toplevel for Parser<'_> {
    fn parse_toplevel_1(&mut self, v1: List) -> Result<List, Error> {
        self.parsetree = v1;
        Ok(List::new())
    }

    fn parse_toplevel_2(&mut self, v2: Option<Box<TypeName>>) -> Result<List, Error> {
        self.parsetree = vec![v2.map(Node::TypeName)];
        Ok(List::new())
    }

    fn parse_toplevel_3(&mut self, v2: Option<Node>, at2: i32) -> Result<List, Error> {
        self.parsetree = vec![Some(makeRawStmt(v2, at2).into())];
        Ok(List::new())
    }

    fn parse_toplevel_4(&mut self, v2: Option<Node>, at2: i32) -> Result<List, Error> {
        self.parsetree = vec![Some(makeRawStmt(assign(v2, 1), at2).into())];
        Ok(List::new())
    }

    fn parse_toplevel_5(&mut self, v2: Option<Node>, at2: i32) -> Result<List, Error> {
        self.parsetree = vec![Some(makeRawStmt(assign(v2, 2), at2).into())];
        Ok(List::new())
    }

    fn parse_toplevel_6(&mut self, v2: Option<Node>, at2: i32) -> Result<List, Error> {
        self.parsetree = vec![Some(makeRawStmt(assign(v2, 3), at2).into())];
        Ok(List::new())
    }
}

/// Sets `nnames` of the `PLAssignStmt` of the `MODE_PLPGSQL_ASSIGN` modes.
fn assign(stmt: Option<Node>, nnames: i32) -> Option<Node> {
    let mut stmt = stmt;
    if let Some(n) = stmt.as_mut().and_then(PLAssignStmt::peek_mut) {
        n.nnames = nnames;
    }
    stmt
}

impl rules::stmtmulti for Parser<'_> {
    fn stmtmulti_1(
        &mut self,
        v1: List,
        v3: Option<Node>,
        at2: i32,
        at3: i32,
    ) -> Result<List, Error> {
        let mut list = v1;
        // The length of the statement before the `;`.
        if let Some(rs) = list.last_mut().and_then(Option::as_mut).and_then(RawStmt::peek_mut) {
            updateRawStmtEnd(rs, at2);
        }
        if v3.is_some() {
            list.push(Some(makeRawStmt(v3, at3).into()));
        }
        Ok(list)
    }
}

impl rules::CheckPointStmt for Parser<'_> {
    fn CheckPointStmt_1(&mut self, v2: List) -> Result<Option<Node>, Error> {
        // C sets the options after `$$ = n`, through the pointer that it still has.
        Ok(Some(CheckPointStmt { options: v2 }.into()))
    }
}

impl rules::PartitionBoundSpec for Parser<'_> {
    fn PartitionBoundSpec_1(
        &mut self,
        v5: List,
        at3: i32,
    ) -> Result<Option<Box<PartitionBoundSpec>>, Error> {
        let mut n = PartitionBoundSpec {
            strategy: PartitionStrategy::PARTITION_STRATEGY_HASH.0 as u8,
            modulus: -1,
            remainder: -1,
            ..Default::default()
        };
        for cell in &v5 {
            let opt = castRef::<DefElem>(cell.as_ref())?;
            let duplicate =
                |message| Err(self.error(ERRCODE_DUPLICATE_OBJECT, message, opt.location));
            match opt.defname.as_deref() {
                Some("modulus") => {
                    if n.modulus != -1 {
                        return duplicate("modulus for hash partition provided more than once");
                    }
                    n.modulus = defGetInt32(opt)?;
                }
                Some("remainder") => {
                    if n.remainder != -1 {
                        return duplicate("remainder for hash partition provided more than once");
                    }
                    n.remainder = defGetInt32(opt)?;
                }
                name => {
                    let name = name.unwrap_or_default();
                    let message =
                        format!("unrecognized hash partition bound specification \"{name}\"");
                    return Err(self.error(ERRCODE_SYNTAX_ERROR, &message, opt.location));
                }
            }
        }
        if n.modulus == -1 {
            let message = "modulus for hash partition must be specified";
            return Err(self.error(ERRCODE_SYNTAX_ERROR, message, at3));
        }
        if n.remainder == -1 {
            let message = "remainder for hash partition must be specified";
            return Err(self.error(ERRCODE_SYNTAX_ERROR, message, at3));
        }
        n.location = at3;
        Ok(Some(Box::new(n)))
    }
}

impl rules::set_clause for Parser<'_> {
    fn set_clause_2(&mut self, mut v2: List, v5: Option<Node>) -> Result<List, Error> {
        let ncolumns = list_length(&v2);
        // Each column gets a `MultiAssignRef` with the same source. C has the pointer to the source in each one, and the Rust has a copy.
        for (colno, col_cell) in (1..).zip(&mut v2) {
            let res_col = castMut::<ResTarget>(col_cell)?;
            let r = MultiAssignRef { source: v5.clone(), colno, ncolumns };
            res_col.val = Some(r.into());
        }
        Ok(v2)
    }
}
