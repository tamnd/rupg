//! The SQL/XML functions of `parse_expr.c` with their own syntax: `transformXmlExpr` and `transformXmlSerialize`.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{Node, XmlExpr as RawXmlExpr, XmlExprOp, XmlOptionType, XmlSerialize};
use rupg_types::{oid, xml_name};

use crate::Analyzer;
use crate::coerce::{AtOpt, Context};
use crate::colname::figure_colname;
use crate::expr::{Expr, ExprKind, XmlExpr, XmlOp};
use crate::typename::place;
use crate::types;

impl Analyzer<'_> {
    /// `transformXmlExpr`.
    pub(crate) fn xml_expr(&mut self, x: &RawXmlExpr) -> Result<Expr> {
        let op = match x.op {
            XmlExprOp::IS_XMLCONCAT => XmlOp::Concat,
            XmlExprOp::IS_XMLELEMENT => XmlOp::Element,
            XmlExprOp::IS_XMLFOREST => XmlOp::Forest,
            XmlExprOp::IS_XMLPARSE => XmlOp::Parse,
            XmlExprOp::IS_XMLPI => XmlOp::Pi,
            XmlExprOp::IS_XMLROOT => XmlOp::Root,
            XmlExprOp::IS_DOCUMENT => XmlOp::Document,
            _ => return Err(Error::internal("an XmlExpr of the grammar")),
        };
        let mut named: Vec<(String, Expr)> = Vec::new();
        for r in &x.named_args {
            let Some(Node::ResTarget(r)) = r else { continue };
            let expr = self.transform(r.val.as_ref())?;
            let name = match (&r.name, &r.val) {
                (Some(name), _) => xml_name(name, false, false),
                (None, Some(Node::ColumnRef(_))) => {
                    xml_name(&figure_colname(r.val.as_ref()), true, false)
                }
                _ => {
                    let what = if op == XmlOp::Element { "attribute" } else { "element" };
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        format!("unnamed XML {what} value must be a column reference"),
                    )
                    .at_opt(place(r.location)));
                }
            };
            if op == XmlOp::Element && named.iter().any(|(n, _)| *n == name) {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!("XML attribute name \"{name}\" appears more than once"),
                )
                .at_opt(place(r.location)));
            }
            named.push((name, expr));
        }
        let mut args = Vec::new();
        for (i, e) in x.args.iter().enumerate() {
            let e = self.transform(e.as_ref())?;
            let e = match (op, i) {
                (XmlOp::Concat, _) => self.coerce_to_specific(e, oid::XML, "XMLCONCAT")?,
                (XmlOp::Element, _) => e,
                (XmlOp::Forest, _) => self.coerce_to_specific(e, oid::XML, "XMLFOREST")?,
                (XmlOp::Parse, 0) => self.coerce_to_specific(e, oid::TEXT, "XMLPARSE")?,
                (XmlOp::Parse, _) => self.coerce_to_boolean(e, "XMLPARSE")?,
                (XmlOp::Pi, _) => self.coerce_to_specific(e, oid::TEXT, "XMLPI")?,
                (XmlOp::Root, 0) => self.coerce_to_specific(e, oid::XML, "XMLROOT")?,
                (XmlOp::Root, 1) => self.coerce_to_specific(e, oid::TEXT, "XMLROOT")?,
                (XmlOp::Root, _) => self.coerce_to_specific(e, oid::INT4, "XMLROOT")?,
                (XmlOp::Serialize | XmlOp::Document, _) => {
                    self.coerce_to_specific(e, oid::XML, "IS DOCUMENT")?
                }
            };
            args.push(e);
        }
        let xml = XmlExpr {
            op,
            name: x.name.as_deref().map(|n| xml_name(n, false, false)),
            named,
            args,
            document: x.xmloption == XmlOptionType::XMLOPTION_DOCUMENT,
            indent: false,
            target: (oid::XML, -1),
        };
        let ty = if op == XmlOp::Document { oid::BOOL } else { oid::XML };
        Ok(Expr::new(ExprKind::Xml(Box::new(xml)), ty).at(place(x.location)))
    }

    /// `transformXmlSerialize`: the `text` of `XMLSERIALIZE`, cast to the type of `AS`.
    pub(crate) fn xml_serialize(&mut self, xs: &XmlSerialize) -> Result<Expr> {
        let arg = self.transform(xs.expr.as_ref())?;
        let arg = self.coerce_to_specific(arg, oid::XML, "XMLSERIALIZE")?;
        let Some(type_name) = xs.typeName.as_deref() else {
            return Err(Error::internal("the type of XMLSERIALIZE"));
        };
        let target = self.type_name(type_name)?;
        let at = place(xs.location);
        let xml = XmlExpr {
            op: XmlOp::Serialize,
            name: None,
            named: Vec::new(),
            args: vec![arg],
            document: xs.xmloption == XmlOptionType::XMLOPTION_DOCUMENT,
            indent: xs.indent,
            target,
        };
        let expr = Expr::new(ExprKind::Xml(Box::new(xml)), oid::TEXT).at(at);
        self.coerce_to_target(expr, target.0, target.1, Context::Implicit, None)?.ok_or_else(|| {
            Error::new(
                SqlState::CANNOT_COERCE,
                format!("cannot cast XMLSERIALIZE result to {}", types::name(target.0)),
            )
            .at_opt(at)
        })
    }
}
