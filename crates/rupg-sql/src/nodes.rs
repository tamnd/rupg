//! The raw parse tree: the node types of PostgreSQL and their text.
//!
//! The types are generated from the vendored headers into `generated/nodes.rs`, with the names and the fields that PostgreSQL gives them, so an action of `gram.y` ports to Rust line by line. This module has what the generator does not write: the names of the field types, the helpers of the writers, the writers of the three node types that `outfuncs.c` writes by hand, and the [`Equal`] of the pointers and the lists.
//!
//! The text of a node is the text of `nodeToStringWithLocations`. PostgreSQL logs that text for the raw parse tree of each statement when `debug_print_raw_parse` is on, so a test can compare the tree of this parser with the tree of PostgreSQL for the same statement.
//!
//! Lifted from `crates/rudb-pgparse/src/nodes.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

pub use crate::generated::nodes::*;

/// The text of a `char *` field.
pub type Str = Box<str>;

/// A `List *` of nodes. `NIL` is the empty list. An element can be `NULL`, as in the `list_make1(NIL)` of `DISTINCT`, and `outfuncs.c` writes it as `<>`.
pub type List = Vec<Option<Node>>;

/// `equal()` of `equalfuncs.c`: two trees are equal when all their fields are equal, except the locations. The derived `PartialEq` compares the locations too, so it is not `equal()`.
pub trait Equal {
    /// The two values are equal, without a look at the locations.
    fn equal(&self, other: &Self) -> bool;
}

impl<T: Equal + ?Sized> Equal for Box<T> {
    fn equal(&self, other: &Self) -> bool {
        (**self).equal(other)
    }
}

/// Two `NULL` pointers are equal, and a `NULL` pointer is not equal to a node.
impl<T: Equal> Equal for Option<T> {
    fn equal(&self, other: &Self) -> bool {
        match (self, other) {
            (None, None) => true,
            (Some(a), Some(b)) => a.equal(b),
            _ => false,
        }
    }
}

impl<T: Equal> Equal for Vec<T> {
    fn equal(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().zip(other).all(|(a, b)| a.equal(b))
    }
}

/// A node type that `outNode` writes in braces.
pub trait Out {
    /// Writes the name of the node type and its fields, without the braces.
    fn out(&self, s: &mut String);

    /// Writes the fields only, each name with `prefix` before it. A node type that is the first field of another, as `CreateStmt` is in `CreateForeignTableStmt`, writes its fields so.
    fn fields(&self, s: &mut String, prefix: &str) {
        let _ = (s, prefix);
    }
}

/// A node type that is a variant of [`Node`], with the casts of `castNode` and `IsA`.
pub trait NodeType: Out + Sized {
    /// The node of this type as a [`Node`].
    fn into_node(self: Box<Self>) -> Node;

    /// The node as this type, or the node back when it has another type.
    fn from_node(node: Node) -> Result<Box<Self>, Node>;

    /// The node as this type, or `None`.
    fn peek(node: &Node) -> Option<&Self>;

    /// The node as this type, or `None`.
    fn peek_mut(node: &mut Node) -> Option<&mut Self>;
}

impl Node {
    /// The text of `nodeToStringWithLocations` for the node.
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        self.write(&mut s);
        s
    }
}

/// The text of `nodeToStringWithLocations` for a list, for example the list of `RawStmt` nodes that the parser gives for a query.
pub fn list_text(list: &List) -> String {
    let mut s = String::new();
    w::items(&mut s, list);
    s
}

impl Out for A_Const {
    fn out(&self, s: &mut String) {
        s.push_str("A_CONST");
        if self.isnull {
            s.push_str(" NULL");
        } else {
            w::node(s, "", "val", self.val.as_ref());
        }
        w::int(s, "", "location", self.location);
    }
}

impl Out for A_Expr {
    fn out(&self, s: &mut String) {
        s.push_str("A_EXPR");
        let kind = match self.kind {
            A_Expr_Kind::AEXPR_OP => "",
            A_Expr_Kind::AEXPR_OP_ANY => " ANY",
            A_Expr_Kind::AEXPR_OP_ALL => " ALL",
            A_Expr_Kind::AEXPR_DISTINCT => " DISTINCT",
            A_Expr_Kind::AEXPR_NOT_DISTINCT => " NOT_DISTINCT",
            A_Expr_Kind::AEXPR_NULLIF => " NULLIF",
            A_Expr_Kind::AEXPR_IN => " IN",
            A_Expr_Kind::AEXPR_LIKE => " LIKE",
            A_Expr_Kind::AEXPR_ILIKE => " ILIKE",
            A_Expr_Kind::AEXPR_SIMILAR => " SIMILAR",
            A_Expr_Kind::AEXPR_BETWEEN => " BETWEEN",
            A_Expr_Kind::AEXPR_NOT_BETWEEN => " NOT_BETWEEN",
            A_Expr_Kind::AEXPR_BETWEEN_SYM => " BETWEEN_SYM",
            A_Expr_Kind::AEXPR_NOT_BETWEEN_SYM => " NOT_BETWEEN_SYM",
            _ => " ?",
        };
        s.push_str(kind);
        w::list(s, "", "name", &self.name);
        w::node(s, "", "lexpr", self.lexpr.as_ref());
        w::node(s, "", "rexpr", self.rexpr.as_ref());
        w::int(s, "", "rexpr_list_start", self.rexpr_list_start);
        w::int(s, "", "rexpr_list_end", self.rexpr_list_end);
        w::int(s, "", "location", self.location);
    }
}

impl Out for BoolExpr {
    fn out(&self, s: &mut String) {
        s.push_str("BOOLEXPR :boolop ");
        s.push_str(match self.boolop {
            BoolExprType::AND_EXPR => "and",
            BoolExprType::OR_EXPR => "or",
            BoolExprType::NOT_EXPR => "not",
            _ => "<>",
        });
        w::list(s, "", "args", &self.args);
        w::int(s, "", "location", self.location);
    }
}

/// The helpers of the writers, one for each `WRITE_..._FIELD` macro of `outfuncs.c`.
pub(crate) mod w {
    use std::fmt::Display;

    use super::{List, Node, Out};

    /// Writes ` :name `, with the prefix of an embedded node type.
    fn label(s: &mut String, prefix: &str, name: &str) {
        s.push_str(" :");
        s.push_str(prefix);
        s.push_str(name);
        s.push(' ');
    }

    /// `WRITE_BOOL_FIELD`.
    pub(crate) fn bool(s: &mut String, prefix: &str, name: &str, value: bool) {
        label(s, prefix, name);
        s.push_str(if value { "true" } else { "false" });
    }

    /// `WRITE_INT_FIELD`, `WRITE_UINT_FIELD`, `WRITE_ENUM_FIELD` and `WRITE_LOCATION_FIELD`.
    pub(crate) fn int(s: &mut String, prefix: &str, name: &str, value: impl Display) {
        label(s, prefix, name);
        let _ = std::fmt::Write::write_fmt(s, format_args!("{value}"));
    }

    /// `WRITE_CHAR_FIELD`: `<>` for the zero character.
    pub(crate) fn char(s: &mut String, prefix: &str, name: &str, value: u8) {
        label(s, prefix, name);
        if value == 0 {
            s.push_str("<>");
        } else {
            let mut buffer = [0; 4];
            token(s, Some(char::from(value).encode_utf8(&mut buffer)));
        }
    }

    /// `WRITE_STRING_FIELD`.
    pub(crate) fn text(s: &mut String, prefix: &str, name: &str, value: Option<&str>) {
        label(s, prefix, name);
        token(s, value);
    }

    /// `WRITE_NODE_FIELD` for a `List *`.
    pub(crate) fn list(s: &mut String, prefix: &str, name: &str, value: &List) {
        label(s, prefix, name);
        items(s, value);
    }

    /// `WRITE_NODE_FIELD` for a `Node *`.
    pub(crate) fn node(s: &mut String, prefix: &str, name: &str, value: Option<&Node>) {
        label(s, prefix, name);
        match value {
            Some(node) => node.write(s),
            None => s.push_str("<>"),
        }
    }

    /// `WRITE_NODE_FIELD` for a pointer to a node type.
    pub(crate) fn boxed<T: Out>(s: &mut String, prefix: &str, name: &str, value: Option<&T>) {
        label(s, prefix, name);
        match value {
            Some(node) => braced(s, node),
            None => s.push_str("<>"),
        }
    }

    /// A node type in braces.
    pub(crate) fn braced<T: Out + ?Sized>(s: &mut String, node: &T) {
        s.push('{');
        node.out(s);
        s.push('}');
    }

    /// `_outList`: the nodes in parentheses with a space between two, or `<>` for `NIL`.
    pub(crate) fn items(s: &mut String, list: &List) {
        if list.is_empty() {
            s.push_str("<>");
            return;
        }
        s.push('(');
        for (i, node) in list.iter().enumerate() {
            if i > 0 {
                s.push(' ');
            }
            match node {
                Some(node) => node.write(s),
                None => s.push_str("<>"),
            }
        }
        s.push(')');
    }

    /// `_outInteger`.
    pub(crate) fn integer(s: &mut String, value: i32) {
        let _ = std::fmt::Write::write_fmt(s, format_args!("{value}"));
    }

    /// `_outString`: the text in double quotes, with the escapes of `outToken`.
    pub(crate) fn string(s: &mut String, value: &str) {
        s.push('"');
        if !value.is_empty() {
            token(s, Some(value));
        }
        s.push('"');
    }

    /// `outToken`: `<>` for `NULL`, `""` for the empty string, and else the text with a backslash before each character that `read.c` treats in a special way.
    pub(crate) fn token(s: &mut String, value: Option<&str>) {
        let Some(value) = value else {
            s.push_str("<>");
            return;
        };
        let bytes = value.as_bytes();
        let Some(&first) = bytes.first() else {
            s.push_str("\"\"");
            return;
        };
        let second = bytes.get(1).copied().unwrap_or(0);
        if first == b'<'
            || first == b'"'
            || first.is_ascii_digit()
            || (matches!(first, b'+' | b'-') && (second.is_ascii_digit() || second == b'.'))
        {
            s.push('\\');
        }
        for c in value.chars() {
            if matches!(c, ' ' | '\n' | '\t' | '(' | ')' | '{' | '}' | '\\') {
                s.push('\\');
            }
            s.push(c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens() {
        let token = |value: Option<&str>| {
            let mut s = String::new();
            w::token(&mut s, value);
            s
        };
        assert_eq!(token(None), "<>");
        assert_eq!(token(Some("")), "\"\"");
        assert_eq!(token(Some("a b(c)")), "a\\ b\\(c\\)");
        assert_eq!(token(Some("1x")), "\\1x");
        assert_eq!(token(Some("-.5")), "\\-.5");
        assert_eq!(token(Some("-a")), "-a");
        assert_eq!(token(Some("<x")), "\\<x");
    }

    fn string(text: &str) -> Option<Node> {
        Some(Node::String(text.into()))
    }

    fn column(name: &str, location: i32) -> Option<Node> {
        Some(ColumnRef { fields: vec![string(name)], location }.into())
    }

    fn integer(value: i32, location: i32) -> Option<Node> {
        Some(A_Const { val: Some(Node::Integer(value)), isnull: false, location }.into())
    }

    /// The tree that PostgreSQL 19 logs with `debug_print_raw_parse` for a query, built by hand.
    #[test]
    fn select() {
        // select a+1 as x, 'q'::text from t where b between 1 and 2 and not c
        let plus = A_Expr {
            kind: A_Expr_Kind::AEXPR_OP,
            name: vec![string("+")],
            lexpr: column("a", 7),
            rexpr: integer(1, 9),
            location: 8,
            ..A_Expr::default()
        };
        let cast = TypeCast {
            arg: Some(A_Const { val: string("q"), isnull: false, location: 17 }.into()),
            typeName: Some(Box::new(TypeName {
                names: vec![string("text")],
                typemod: -1,
                location: 22,
                ..TypeName::default()
            })),
            location: 20,
        };
        let between = A_Expr {
            kind: A_Expr_Kind::AEXPR_BETWEEN,
            name: vec![string("BETWEEN")],
            lexpr: column("b", 40),
            rexpr: Some(Node::List(vec![integer(1, 50), integer(2, 56)])),
            location: 42,
            ..A_Expr::default()
        };
        let not =
            BoolExpr { boolop: BoolExprType::NOT_EXPR, args: vec![column("c", 66)], location: 62 };
        let select = SelectStmt {
            targetList: vec![
                Some(
                    ResTarget {
                        name: Some("x".into()),
                        val: Some(plus.into()),
                        location: 7,
                        ..ResTarget::default()
                    }
                    .into(),
                ),
                Some(
                    ResTarget { val: Some(cast.into()), location: 17, ..ResTarget::default() }
                        .into(),
                ),
            ],
            fromClause: vec![Some(
                RangeVar {
                    relname: Some("t".into()),
                    inh: true,
                    relpersistence: b'p',
                    location: 32,
                    ..RangeVar::default()
                }
                .into(),
            )],
            whereClause: Some(
                BoolExpr {
                    boolop: BoolExprType::AND_EXPR,
                    args: vec![Some(between.into()), Some(not.into())],
                    location: 58,
                }
                .into(),
            ),
            ..SelectStmt::default()
        };
        let raw = RawStmt { stmt: Some(select.into()), stmt_location: 0, stmt_len: 0 };
        let expected = "({RAWSTMT :stmt {SELECTSTMT :distinctClause <> :intoClause <> :targetList \
            ({RESTARGET :name x :indirection <> :val {A_EXPR :name (\"+\") :lexpr {COLUMNREF \
            :fields (\"a\") :location 7} :rexpr {A_CONST :val 1 :location 9} :rexpr_list_start 0 \
            :rexpr_list_end 0 :location 8} :location 7} {RESTARGET :name <> :indirection <> :val \
            {TYPECAST :arg {A_CONST :val \"q\" :location 17} :typeName {TYPENAME :names \
            (\"text\") :typeOid 0 :setof false :pct_type false :typmods <> :typemod -1 \
            :arrayBounds <> :location 22} :location 20} :location 17}) :fromClause ({RANGEVAR \
            :catalogname <> :schemaname <> :relname t :inh true :relpersistence p :alias <> \
            :location 32}) :whereClause {BOOLEXPR :boolop and :args ({A_EXPR BETWEEN :name \
            (\"BETWEEN\") :lexpr {COLUMNREF :fields (\"b\") :location 40} :rexpr ({A_CONST :val \
            1 :location 50} {A_CONST :val 2 :location 56}) :rexpr_list_start 0 :rexpr_list_end 0 \
            :location 42} {BOOLEXPR :boolop not :args ({COLUMNREF :fields (\"c\") :location 66}) \
            :location 62}) :location 58} :groupClause <> :groupDistinct false :havingClause <> \
            :windowClause <> :valuesLists <> :sortClause <> :limitOffset <> :limitCount <> \
            :limitOption 0 :lockingClause <> :withClause <> :op 0 :all false :larg <> :rarg <>} \
            :stmt_location 0 :stmt_len 0})";
        assert_eq!(list_text(&vec![Some(raw.into())]), expected);
    }

    /// A node type that is the first field of another writes its fields with `base.` before them.
    #[test]
    fn embedded() {
        // create foreign table f (a int) server s
        let column = ColumnDef {
            colname: Some("a".into()),
            typeName: Some(Box::new(TypeName {
                names: vec![string("pg_catalog"), string("int4")],
                typemod: -1,
                location: 26,
                ..TypeName::default()
            })),
            is_local: true,
            location: 24,
            ..ColumnDef::default()
        };
        let create = CreateForeignTableStmt {
            base: CreateStmt {
                relation: Some(Box::new(RangeVar {
                    relname: Some("f".into()),
                    inh: true,
                    relpersistence: b'p',
                    location: 21,
                    ..RangeVar::default()
                })),
                tableElts: vec![Some(column.into())],
                ..CreateStmt::default()
            },
            servername: Some("s".into()),
            options: Vec::new(),
        };
        let expected = "{CREATEFOREIGNTABLESTMT :base.relation {RANGEVAR :catalogname <> :schemaname <> \
            :relname f :inh true :relpersistence p :alias <> :location 21} :base.tableElts \
            ({COLUMNDEF :colname a :typeName {TYPENAME :names (\"pg_catalog\" \"int4\") :typeOid 0 \
            :setof false :pct_type false :typmods <> :typemod -1 :arrayBounds <> :location 26} \
            :compression <> :inhcount 0 :is_local true :is_not_null false :is_from_type false \
            :storage <> :storage_name <> :raw_default <> :cooked_default <> :identity <> \
            :identitySequence <> :generated <> :collClause <> :collOid 0 :constraints <> \
            :fdwoptions <> :location 24}) :base.inhRelations <> :base.partbound <> :base.partspec \
            <> :base.ofTypename <> :base.constraints <> :base.nnconstraints <> :base.options <> \
            :base.oncommit 0 :base.tablespacename <> :base.accessMethod <> :base.if_not_exists \
            false :servername s :options <>}";
        assert_eq!(Node::from(create).to_text(), expected);
    }
}
