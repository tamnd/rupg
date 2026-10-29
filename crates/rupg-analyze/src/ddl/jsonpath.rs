//! `jspIsMutable`: whether a constant path of `JSON_EXISTS`, `JSON_QUERY` or `JSON_VALUE` gives a result that depends on the settings of the session.

use rupg_types::oid;
use rupg_types::{JsonPath, PathItem, PathOp};

use crate::Env;
use crate::expr::Expr;

/// `JsonPathDatatypeStatus`: what the items of a path give, as far as the datetime types go.
#[derive(Clone, Copy, PartialEq)]
enum Status {
    /// `null`, a boolean, a number, a string, an array or an object.
    NonDateTime,
    /// A datetime value of a type that is not known.
    UnknownDateTime,
    /// `timetz` or `timestamptz`.
    Zoned,
    /// `time`, `timestamp` or `date`.
    NonZoned,
}

/// `JsonPathMutableContext`.
struct Walker<'a> {
    env: &'a dyn Env,
    passing: &'a [(String, Expr)],
    /// The status of the item `@`.
    current: Status,
    lax: bool,
    mutable: bool,
}

/// `jspIsMutable`: true when the path calls a datetime method that reads the time zone of the session, or compares datetime values of which one has a zone and one has none.
pub(super) fn is_mutable(env: &dyn Env, path: &JsonPath, passing: &[(String, Expr)]) -> bool {
    let mut walker =
        Walker { env, passing, current: Status::NonDateTime, lax: path.lax, mutable: false };
    walker.walk(&path.expr);
    walker.mutable
}

impl Walker<'_> {
    /// `jspIsMutableWalker`: the status of the last item of the chain.
    fn walk(&mut self, chain: &[PathItem]) -> Status {
        let mut status = Status::NonDateTime;
        for item in chain {
            if self.mutable {
                break;
            }
            match item {
                PathItem::Root
                | PathItem::Null
                | PathItem::String(_)
                | PathItem::Numeric(_)
                | PathItem::Bool(_) => {}
                PathItem::Current => status = self.current,
                PathItem::Unary(PathOp::Filter, arg) => {
                    let outer = self.current;
                    self.current = status;
                    self.walk(arg);
                    self.current = outer;
                }
                PathItem::Variable(name) => {
                    // As `strncmp` with the length of the name of the path, the name of the `PASSING` clause can be longer.
                    let found = self.passing.iter().find(|(n, _)| n.starts_with(name.as_str()));
                    if let Some((_, value)) = found {
                        status = match value.ty {
                            oid::DATE | oid::TIME | oid::TIMESTAMP => Status::NonZoned,
                            oid::TIMETZ | oid::TIMESTAMPTZ => Status::Zoned,
                            _ => Status::NonDateTime,
                        };
                    }
                }
                PathItem::Binary(
                    PathOp::Equal
                    | PathOp::NotEqual
                    | PathOp::Less
                    | PathOp::Greater
                    | PathOp::LessOrEqual
                    | PathOp::GreaterOrEqual,
                    left,
                    right,
                ) => {
                    let left = self.walk(left);
                    let right = self.walk(right);
                    // A comparison of datetime values with a different zone status is mutable.
                    if left != Status::NonDateTime
                        && right != Status::NonDateTime
                        && (left == Status::UnknownDateTime
                            || right == Status::UnknownDateTime
                            || left != right)
                    {
                        self.mutable = true;
                    }
                }
                PathItem::Unary(_, arg) | PathItem::LikeRegex { expr: arg, .. } => {
                    self.walk(arg);
                }
                PathItem::Binary(_, left, right) => {
                    self.walk(left);
                    self.walk(right);
                }
                PathItem::IndexArray(subscripts) => {
                    for (from, to) in subscripts {
                        if let Some(to) = to {
                            self.walk(to);
                        }
                        self.walk(from);
                    }
                    if !self.lax {
                        status = Status::NonDateTime;
                    }
                }
                PathItem::AnyArray => {
                    if !self.lax {
                        status = Status::NonDateTime;
                    }
                }
                PathItem::Any { first, .. } => {
                    if *first > 0 {
                        status = Status::NonDateTime;
                    }
                }
                PathItem::Method(PathOp::Datetime, args) => {
                    status = match args.first() {
                        Some(PathItem::String(template)) => {
                            if self.env.datetime_format_has_tz(template) {
                                Status::Zoned
                            } else {
                                Status::NonZoned
                            }
                        }
                        // A template that is not a string is an error at run time.
                        Some(_) => Status::NonDateTime,
                        None => Status::UnknownDateTime,
                    };
                }
                PathItem::Method(
                    PathOp::Time
                    | PathOp::Date
                    | PathOp::Timestamp
                    | PathOp::TimeTz
                    | PathOp::TimestampTz,
                    _,
                ) => {
                    status = Status::NonZoned;
                    self.mutable = true;
                }
                PathItem::Key(_) | PathItem::AnyKey | PathItem::Last | PathItem::Method(..) => {
                    status = Status::NonDateTime;
                }
            }
        }
        status
    }
}
