//! The list and cast macros of `pg_list.h` and `nodes.h` that the actions of `gram.y` use.
//!
//! In C a cast such as `(SelectStmt *) $1` or `castNode(Alias, $2)` trusts the grammar for the type of the node. Here a node of another type is an internal error, not a crash.
//!
//! Lifted from `crates/rudb-pgparse/src/actions/list.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use crate::error::Error;
use crate::nodes::*;

/// `castNode`: the node as the type `T`, or `None` for `NULL`.
pub(crate) fn castNode<T: NodeType>(node: Option<Node>) -> Result<Option<Box<T>>, Error> {
    match node {
        None => Ok(None),
        Some(node) => T::from_node(node)
            .map(Some)
            .map_err(|_| Error::internal("castNode of a node of the wrong type")),
    }
}

/// `castNode(List, node)`: the list in a node, or `NIL` for `NULL`.
pub(crate) fn castList(node: Option<Node>) -> Result<List, Error> {
    match node {
        None => Ok(List::new()),
        Some(Node::List(list)) => Ok(list),
        Some(_) => Err(Error::internal("castNode of a node that is not a List")),
    }
}

/// The struct that a pointer points to, for `$1->field` in C. A `NULL` pointer is an internal error, where the C would crash.
pub(crate) fn pointee<T>(pointer: &Option<Box<T>>) -> Result<&T, Error> {
    pointer.as_deref().ok_or_else(|| Error::internal("a NULL pointer in an action"))
}

/// The struct that a pointer points to, for `$1->field = value` in C.
pub(crate) fn pointee_mut<T>(pointer: &mut Option<Box<T>>) -> Result<&mut T, Error> {
    pointer.as_deref_mut().ok_or_else(|| Error::internal("a NULL pointer in an action"))
}

/// A pointer that the grammar never makes `NULL`, as `T *n = $1` in C has it.
pub(crate) fn pointer<T>(pointer: Option<Box<T>>) -> Result<Box<T>, Error> {
    pointer.ok_or_else(|| Error::internal("a NULL pointer in an action"))
}

/// `(T *) node` for a node that the grammar always makes of the type `T`.
pub(crate) fn castMut<T: NodeType>(node: &mut Option<Node>) -> Result<&mut T, Error> {
    node.as_mut()
        .and_then(T::peek_mut)
        .ok_or_else(|| Error::internal("a cast of a node of the wrong type"))
}

/// `(T *) node` for a node that the grammar always makes of the type `T`, to read its fields.
pub(crate) fn castRef<T: NodeType>(node: Option<&Node>) -> Result<&T, Error> {
    node.and_then(T::peek).ok_or_else(|| Error::internal("a cast of a node of the wrong type"))
}

/// `linitial`. An element that the list does not have is `NULL`, where the C would crash.
pub(crate) fn linitial(list: &List) -> Option<&Node> {
    list.first().and_then(Option::as_ref)
}

/// `lsecond`.
pub(crate) fn lsecond(list: &List) -> Option<&Node> {
    list.get(1).and_then(Option::as_ref)
}

/// `llast`.
pub(crate) fn llast(list: &List) -> Option<&Node> {
    list.last().and_then(Option::as_ref)
}

/// `list_length`, which is never more than `i32::MAX` for a list of the parser.
pub(crate) fn list_length(list: &List) -> i32 {
    i32::try_from(list.len()).unwrap_or(i32::MAX)
}

/// A list as a node: `(Node *) list`. `NIL` is `NULL`.
pub(crate) fn listNode(list: List) -> Option<Node> {
    (!list.is_empty()).then_some(Node::List(list))
}

/// `list_make1`.
pub(crate) fn list_make1(x1: Option<Node>) -> List {
    vec![x1]
}

/// `list_make2`.
pub(crate) fn list_make2(x1: Option<Node>, x2: Option<Node>) -> List {
    vec![x1, x2]
}

/// `lcons`: the list with `datum` in front.
pub(crate) fn lcons(datum: Option<Node>, mut list: List) -> List {
    list.insert(0, datum);
    list
}

/// `lappend`.
pub(crate) fn lappend(mut list: List, datum: Option<Node>) -> List {
    list.push(datum);
    list
}

/// `list_concat`.
pub(crate) fn list_concat(mut list1: List, mut list2: List) -> List {
    list1.append(&mut list2);
    list1
}

/// `linitial`, `lsecond` and so on, which take the first `N` elements of the list. An element that the list does not have is `NULL`.
pub(crate) fn elements<const N: usize>(list: List) -> [Option<Node>; N] {
    let mut items = list.into_iter();
    // flatten: an element past the end and a `NULL` element are both `None`, not a column.
    std::array::from_fn(|_| items.next().flatten())
}

/// `$$ = $k;` and then changes to the fields of `$$`. The node of `$k` is never `NULL` there in PostgreSQL, and a `None` stays `None`.
pub(crate) fn change<T>(mut node: Option<Box<T>>, f: impl FnOnce(&mut T)) -> Option<Box<T>> {
    if let Some(n) = node.as_mut() {
        f(n);
    }
    node
}
