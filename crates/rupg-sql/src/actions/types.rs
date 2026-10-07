//! The actions of the type names that the translator cannot write: the generic, bit and character types, the interval fields, and the interval of `SET TIME ZONE`.
//!
//! Lifted from `crates/rudb-pgparse/src/actions/types.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use super::*;
use crate::error::Error;
use crate::generated::glue::rules;
use crate::nodes::*;

/// `INTERVAL_MASK` of `timestamp.h`: the bit of an interval field in an interval typmod.
fn INTERVAL_MASK(b: i32) -> i32 {
    1 << b
}

/// `SystemTypeName(name)` with the location `location`.
fn system_type(name: &str, location: i32) -> Option<Box<TypeName>> {
    Some(Box::new(TypeName { location, ..SystemTypeName(name) }))
}

/// `SystemTypeName(name)` with the type modifiers `typmods` and the location `location`.
fn system_type_mods(name: &str, typmods: List, location: i32) -> Option<Box<TypeName>> {
    Some(Box::new(TypeName { typmods, location, ..SystemTypeName(name) }))
}

impl rules::GenericType for Parser<'_> {
    fn GenericType_1(
        &mut self,
        v1: Option<Str>,
        v2: List,
        at1: i32,
    ) -> Result<Option<Box<TypeName>>, Error> {
        let t = makeTypeName(v1.as_deref().unwrap_or_default());
        Ok(Some(Box::new(TypeName { typmods: v2, location: at1, ..t })))
    }
}

impl rules::BitWithLength for Parser<'_> {
    fn BitWithLength_1(
        &mut self,
        v2: bool,
        v4: List,
        at1: i32,
    ) -> Result<Option<Box<TypeName>>, Error> {
        let typname = if v2 { "varbit" } else { "bit" };
        Ok(system_type_mods(typname, v4, at1))
    }
}

impl rules::CharacterWithLength for Parser<'_> {
    fn CharacterWithLength_1(
        &mut self,
        v1: Option<Str>,
        v3: i32,
        at1: i32,
        at3: i32,
    ) -> Result<Option<Box<TypeName>>, Error> {
        let typmods = list_make1(Some(makeIntConst(v3, at3)));
        Ok(system_type_mods(v1.as_deref().unwrap_or_default(), typmods, at1))
    }
}

impl rules::CharacterWithoutLength for Parser<'_> {
    fn CharacterWithoutLength_1(
        &mut self,
        v1: Option<Str>,
        at1: i32,
    ) -> Result<Option<Box<TypeName>>, Error> {
        // `char` is `char(1)`, and `varchar` has no limit.
        let name = v1.as_deref().unwrap_or_default();
        if name == "bpchar" {
            Ok(system_type_mods(name, list_make1(Some(makeIntConst(1, -1))), at1))
        } else {
            Ok(system_type(name, at1))
        }
    }
}

/// `list_make1(makeIntConst(mask, location))`, the typmods of an interval with fields and no precision.
fn interval(mask: i32, location: i32) -> List {
    list_make1(Some(makeIntConst(mask, location)))
}

/// `$$ = $3; linitial($$) = makeIntConst(mask, location);`: the fields of an interval that ends in `SECOND`, where `interval_second` gave the mask of `SECOND` and maybe a precision.
fn interval_to_second(mut second: List, mask: i32, location: i32) -> List {
    if let Some(first) = second.first_mut() {
        *first = Some(makeIntConst(mask, location));
    }
    second
}

impl rules::opt_interval for Parser<'_> {
    fn opt_interval_1(&mut self, at1: i32) -> Result<List, Error> {
        Ok(interval(INTERVAL_MASK(YEAR), at1))
    }

    fn opt_interval_2(&mut self, at1: i32) -> Result<List, Error> {
        Ok(interval(INTERVAL_MASK(MONTH), at1))
    }

    fn opt_interval_3(&mut self, at1: i32) -> Result<List, Error> {
        Ok(interval(INTERVAL_MASK(DAY), at1))
    }

    fn opt_interval_4(&mut self, at1: i32) -> Result<List, Error> {
        Ok(interval(INTERVAL_MASK(HOUR), at1))
    }

    fn opt_interval_5(&mut self, at1: i32) -> Result<List, Error> {
        Ok(interval(INTERVAL_MASK(MINUTE), at1))
    }

    fn opt_interval_7(&mut self, at1: i32) -> Result<List, Error> {
        Ok(interval(INTERVAL_MASK(YEAR) | INTERVAL_MASK(MONTH), at1))
    }

    fn opt_interval_8(&mut self, at1: i32) -> Result<List, Error> {
        Ok(interval(INTERVAL_MASK(DAY) | INTERVAL_MASK(HOUR), at1))
    }

    fn opt_interval_9(&mut self, at1: i32) -> Result<List, Error> {
        Ok(interval(INTERVAL_MASK(DAY) | INTERVAL_MASK(HOUR) | INTERVAL_MASK(MINUTE), at1))
    }

    fn opt_interval_10(&mut self, v3: List, at1: i32) -> Result<List, Error> {
        let mask = INTERVAL_MASK(DAY)
            | INTERVAL_MASK(HOUR)
            | INTERVAL_MASK(MINUTE)
            | INTERVAL_MASK(SECOND);
        Ok(interval_to_second(v3, mask, at1))
    }

    fn opt_interval_11(&mut self, at1: i32) -> Result<List, Error> {
        Ok(interval(INTERVAL_MASK(HOUR) | INTERVAL_MASK(MINUTE), at1))
    }

    fn opt_interval_12(&mut self, v3: List, at1: i32) -> Result<List, Error> {
        let mask = INTERVAL_MASK(HOUR) | INTERVAL_MASK(MINUTE) | INTERVAL_MASK(SECOND);
        Ok(interval_to_second(v3, mask, at1))
    }

    fn opt_interval_13(&mut self, v3: List, at1: i32) -> Result<List, Error> {
        Ok(interval_to_second(v3, INTERVAL_MASK(MINUTE) | INTERVAL_MASK(SECOND), at1))
    }
}

impl rules::interval_second for Parser<'_> {
    fn interval_second_1(&mut self, at1: i32) -> Result<List, Error> {
        Ok(interval(INTERVAL_MASK(SECOND), at1))
    }

    fn interval_second_2(&mut self, v3: i32, at1: i32, at3: i32) -> Result<List, Error> {
        Ok(list_make2(Some(makeIntConst(INTERVAL_MASK(SECOND), at1)), Some(makeIntConst(v3, at3))))
    }
}

impl rules::zone_value for Parser<'_> {
    fn zone_value_3(
        &mut self,
        v1: Option<Box<TypeName>>,
        v2: Option<Str>,
        v3: List,
        at2: i32,
        at3: i32,
    ) -> Result<Option<Node>, Error> {
        let mut t = v1;
        if !v3.is_empty() {
            let n = castRef::<A_Const>(linitial(&v3))?;
            if (intVal(n.val.as_ref())? & !(INTERVAL_MASK(HOUR) | INTERVAL_MASK(MINUTE))) != 0 {
                let message = "time zone interval must be HOUR or HOUR TO MINUTE";
                return Err(self.error(ERRCODE_SYNTAX_ERROR, message, at3));
            }
        }
        pointee_mut(&mut t)?.typmods = v3;
        Ok(Some(makeStringConstCast(v2, at2, t)))
    }
}
