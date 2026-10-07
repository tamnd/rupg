//! The actions of the expressions that the translator cannot write: the alternatives of `a_expr`, `c_expr`, `func_expr` and `func_expr_common_subexpr` with a loop or a `switch`, the window frames, `CASE` and the function arguments.
//!
//! Lifted from `crates/rudb-pgparse/src/actions/expr.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use super::*;
use crate::error::Error;
use crate::generated::glue::rules;
use crate::nodes::*;

/// A `NullTest` of `arg`.
fn null_test(
    arg: Option<Node>,
    nulltesttype: NullTestType,
    location: i32,
) -> Result<Option<Node>, Error> {
    Ok(Some(NullTest { arg, nulltesttype, location, ..NullTest::default() }.into()))
}

/// A `BooleanTest` of `arg`.
fn boolean_test(
    arg: Option<Node>,
    booltesttype: BoolTestType,
    location: i32,
) -> Result<Option<Node>, Error> {
    Ok(Some(BooleanTest { arg, booltesttype, location }.into()))
}

impl rules::a_expr for Parser<'_> {
    fn a_expr_38(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        null_test(v1, NullTestType::IS_NULL, at2)
    }

    fn a_expr_39(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        null_test(v1, NullTestType::IS_NULL, at2)
    }

    fn a_expr_40(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        null_test(v1, NullTestType::IS_NOT_NULL, at2)
    }

    fn a_expr_41(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        null_test(v1, NullTestType::IS_NOT_NULL, at2)
    }

    fn a_expr_43(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        boolean_test(v1, BoolTestType::IS_TRUE, at2)
    }

    fn a_expr_44(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        boolean_test(v1, BoolTestType::IS_NOT_TRUE, at2)
    }

    fn a_expr_45(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        boolean_test(v1, BoolTestType::IS_FALSE, at2)
    }

    fn a_expr_46(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        boolean_test(v1, BoolTestType::IS_NOT_FALSE, at2)
    }

    fn a_expr_47(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        boolean_test(v1, BoolTestType::IS_UNKNOWN, at2)
    }

    fn a_expr_48(&mut self, v1: Option<Node>, at2: i32) -> Result<Option<Node>, Error> {
        boolean_test(v1, BoolTestType::IS_NOT_UNKNOWN, at2)
    }
}

/// `arg` with the subscripts and the field selections `indirection`, or `arg` alone when there are none.
fn indirect(
    arg: Option<Node>,
    indirection: List,
    yyscanner: &Parser<'_>,
) -> Result<Option<Node>, Error> {
    if indirection.is_empty() {
        return Ok(arg);
    }
    let indirection = check_indirection(indirection, yyscanner)?;
    Ok(Some(A_Indirection { arg, indirection }.into()))
}

impl rules::c_expr for Parser<'_> {
    fn c_expr_3(&mut self, v1: i32, v2: List, at1: i32) -> Result<Option<Node>, Error> {
        let p = ParamRef { number: v1, location: at1 };
        indirect(Some(p.into()), v2, self)
    }
}

/// `makeFuncCall(name, args, COERCE_EXPLICIT_CALL, location)`, a plain function call.
fn call(name: List, args: List, location: i32) -> FuncCall {
    makeFuncCall(name, args, CoercionForm::COERCE_EXPLICIT_CALL, location)
}

impl rules::func_expr for Parser<'_> {
    fn func_expr_2(
        &mut self,
        v1: Option<Node>,
        v2: Option<Node>,
        v3: Option<Box<WindowDef>>,
    ) -> Result<Option<Node>, Error> {
        let mut v1 = v1;
        let constructor = match v1.as_mut() {
            Some(Node::JsonObjectAgg(n)) => &mut n.constructor,
            Some(Node::JsonArrayAgg(n)) => &mut n.constructor,
            _ => return Err(Error::internal("a cast of a node of the wrong type")),
        };
        if let Some(n) = constructor {
            n.agg_filter = v2;
            n.over = v3;
        }
        Ok(v1)
    }
}

impl rules::func_expr_common_subexpr for Parser<'_> {
    fn func_expr_common_subexpr_27(
        &mut self,
        v3: Option<Node>,
        v5: Option<Box<TypeName>>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        // `TREAT(expr AS target)` is a call of the function with the name of the type, which allows stronger coercions than the implicit casts.
        let name = v5.and_then(|t| strVal(t.names.last().and_then(Option::as_ref)));
        let name = SystemFuncName(name.as_deref().unwrap_or_default());
        Ok(Some(call(name, list_make1(v3), at1).into()))
    }

    fn func_expr_common_subexpr_34(&mut self, v3: List, at1: i32) -> Result<Option<Node>, Error> {
        let v = MinMaxExpr {
            args: v3,
            op: MinMaxOp::IS_GREATEST,
            location: at1,
            ..MinMaxExpr::default()
        };
        Ok(Some(v.into()))
    }

    fn func_expr_common_subexpr_35(&mut self, v3: List, at1: i32) -> Result<Option<Node>, Error> {
        let v =
            MinMaxExpr { args: v3, op: MinMaxOp::IS_LEAST, location: at1, ..MinMaxExpr::default() };
        Ok(Some(v.into()))
    }

    fn func_expr_common_subexpr_55(
        &mut self,
        v3: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(JsonScalarExpr { expr: v3, output: None, location: at1 }.into()))
    }
}

/// `opt_frame_clause` of the mode `mode`: the frame of `frame_extent` with the mode and the exclusion.
fn frame(n: Option<Box<WindowDef>>, mode: i32, exclusion: i32) -> Option<Box<WindowDef>> {
    change(n, |n| n.frameOptions |= FRAMEOPTION_NONDEFAULT | mode | exclusion)
}

impl rules::opt_frame_clause for Parser<'_> {
    fn opt_frame_clause_1(
        &mut self,
        v2: Option<Box<WindowDef>>,
        v3: i32,
    ) -> Result<Option<Box<WindowDef>>, Error> {
        Ok(frame(v2, FRAMEOPTION_RANGE, v3))
    }

    fn opt_frame_clause_2(
        &mut self,
        v2: Option<Box<WindowDef>>,
        v3: i32,
    ) -> Result<Option<Box<WindowDef>>, Error> {
        Ok(frame(v2, FRAMEOPTION_ROWS, v3))
    }

    fn opt_frame_clause_3(
        &mut self,
        v2: Option<Box<WindowDef>>,
        v3: i32,
    ) -> Result<Option<Box<WindowDef>>, Error> {
        Ok(frame(v2, FRAMEOPTION_GROUPS, v3))
    }
}

impl rules::frame_extent for Parser<'_> {
    fn frame_extent_1(
        &mut self,
        v1: Option<Box<WindowDef>>,
        at1: i32,
    ) -> Result<Option<Box<WindowDef>>, Error> {
        let mut v1 = v1;
        if let Some(n) = v1.as_mut() {
            // Reject the cases that are not valid.
            if n.frameOptions & FRAMEOPTION_START_UNBOUNDED_FOLLOWING != 0 {
                let message = "frame start cannot be UNBOUNDED FOLLOWING";
                return Err(self.error(ERRCODE_WINDOWING_ERROR, message, at1));
            }
            if n.frameOptions & FRAMEOPTION_START_OFFSET_FOLLOWING != 0 {
                let message = "frame starting from following row cannot end with current row";
                return Err(self.error(ERRCODE_WINDOWING_ERROR, message, at1));
            }
            n.frameOptions |= FRAMEOPTION_END_CURRENT_ROW;
        }
        Ok(v1)
    }

    fn frame_extent_2(
        &mut self,
        v2: Option<Box<WindowDef>>,
        v4: Option<Box<WindowDef>>,
        at2: i32,
        at4: i32,
    ) -> Result<Option<Box<WindowDef>>, Error> {
        let mut n1 = v2.unwrap_or_default();
        let n2 = v4.unwrap_or_default();
        // The shift makes the `START_` options of the end bound into `END_` options.
        let frameOptions = n1.frameOptions | n2.frameOptions << 1 | FRAMEOPTION_BETWEEN;
        // Reject the cases that are not valid.
        let error = |message, location| Err(self.error(ERRCODE_WINDOWING_ERROR, message, location));
        if frameOptions & FRAMEOPTION_START_UNBOUNDED_FOLLOWING != 0 {
            return error("frame start cannot be UNBOUNDED FOLLOWING", at2);
        }
        if frameOptions & FRAMEOPTION_END_UNBOUNDED_PRECEDING != 0 {
            return error("frame end cannot be UNBOUNDED PRECEDING", at4);
        }
        if frameOptions & FRAMEOPTION_START_CURRENT_ROW != 0
            && frameOptions & FRAMEOPTION_END_OFFSET_PRECEDING != 0
        {
            return error("frame starting from current row cannot have preceding rows", at4);
        }
        if frameOptions & FRAMEOPTION_START_OFFSET_FOLLOWING != 0
            && frameOptions & (FRAMEOPTION_END_OFFSET_PRECEDING | FRAMEOPTION_END_CURRENT_ROW) != 0
        {
            return error("frame starting from following row cannot have preceding rows", at4);
        }
        n1.frameOptions = frameOptions;
        n1.endOffset = n2.startOffset;
        Ok(Some(n1))
    }
}

impl rules::func_arg_expr for Parser<'_> {
    fn func_arg_expr_2(
        &mut self,
        v1: Option<Str>,
        v3: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        // The analysis sets the argument number.
        Ok(Some(NamedArgExpr { name: v1, arg: v3, argnumber: -1, location: at1 }.into()))
    }

    fn func_arg_expr_3(
        &mut self,
        v1: Option<Str>,
        v3: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(NamedArgExpr { name: v1, arg: v3, argnumber: -1, location: at1 }.into()))
    }
}

impl rules::case_expr for Parser<'_> {
    fn case_expr_1(
        &mut self,
        v2: Option<Node>,
        v3: List,
        v4: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        // The analysis sets the type.
        let c = CaseExpr { arg: v2, args: v3, defresult: v4, location: at1, ..CaseExpr::default() };
        Ok(Some(c.into()))
    }
}

impl rules::when_clause for Parser<'_> {
    fn when_clause_1(
        &mut self,
        v2: Option<Node>,
        v4: Option<Node>,
        at1: i32,
    ) -> Result<Option<Node>, Error> {
        Ok(Some(CaseWhen { expr: v2, result: v4, location: at1 }.into()))
    }
}
