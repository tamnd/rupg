//! The actions of the names and the constants that the translator cannot write: the literal constants, the role names and the negative float constants of `NumericOnly`.
//!
//! Lifted from `crates/rudb-pgparse/src/actions/names.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use super::*;
use crate::error::Error;
use crate::generated::glue::rules;
use crate::nodes::*;

impl rules::AexprConst for Parser<'_> {
    #[allow(clippy::too_many_arguments)]
    fn AexprConst_7(
        &mut self,
        v1: List,
        v3: List,
        v4: List,
        v6: Option<Str>,
        at1: i32,
        at4: i32,
        at6: i32,
    ) -> Result<Option<Node>, Error> {
        // The same syntax with a type modifier. The rule uses `func_arg_list` and `opt_sort_clause` to prevent reduce/reduce conflicts, but the names of the arguments and `ORDER BY` are not allowed here.
        for arg in v3.iter().flatten() {
            if let Node::NamedArgExpr(arg) = arg {
                let message = "type modifier cannot have parameter name";
                return Err(self.error(ERRCODE_SYNTAX_ERROR, message, arg.location));
            }
        }
        if !v4.is_empty() {
            return Err(self.error(
                ERRCODE_SYNTAX_ERROR,
                "type modifier cannot have ORDER BY",
                at4,
            ));
        }
        let t = TypeName { typmods: v3, location: at1, ..makeTypeNameFromNameList(v1) };
        Ok(Some(makeStringConstCast(v6, at6, Some(Box::new(t)))))
    }
}

impl rules::RoleId for Parser<'_> {
    fn RoleId_1(&mut self, v1: Option<Box<RoleSpec>>, at1: i32) -> Result<Option<Str>, Error> {
        let spc = v1.map(|s| *s).unwrap_or_default();
        let message = match spc.roletype {
            RoleSpecType::ROLESPEC_CSTRING => return Ok(spc.rolename),
            RoleSpecType::ROLESPEC_PUBLIC => "role name \"public\" is reserved",
            RoleSpecType::ROLESPEC_SESSION_USER => {
                "SESSION_USER cannot be used as a role name here"
            }
            RoleSpecType::ROLESPEC_CURRENT_USER => {
                "CURRENT_USER cannot be used as a role name here"
            }
            RoleSpecType::ROLESPEC_CURRENT_ROLE => {
                "CURRENT_ROLE cannot be used as a role name here"
            }
            _ => return Ok(None),
        };
        Err(self.error(ERRCODE_RESERVED_NAME, message, at1))
    }
}

impl rules::RoleSpec for Parser<'_> {
    fn RoleSpec_1(&mut self, v1: Option<Str>, at1: i32) -> Result<Option<Box<RoleSpec>>, Error> {
        // `public` and `none` are not keywords, but they have a special meaning here.
        let n = match v1.as_deref() {
            Some("public") => makeRoleSpec(RoleSpecType::ROLESPEC_PUBLIC, at1),
            Some("none") => {
                return Err(self.error(
                    ERRCODE_RESERVED_NAME,
                    "role name \"none\" is reserved",
                    at1,
                ));
            }
            _ => RoleSpec { rolename: v1, ..makeRoleSpec(RoleSpecType::ROLESPEC_CSTRING, at1) },
        };
        Ok(Some(Box::new(n)))
    }
}

impl rules::NumericOnly for Parser<'_> {
    fn NumericOnly_3(&mut self, v2: Option<Str>) -> Result<Option<Node>, Error> {
        let mut f = v2.unwrap_or_default();
        doNegateFloat(&mut f);
        Ok(Some(makeFloat(Some(f))))
    }
}
