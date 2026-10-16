//! `ts_rewrite` of `tsquery_rewrite.c` with the tree of a query of `tsquery_util.c`: the form with a target and a substitute.

use std::cmp::Ordering;
use std::convert::Infallible;

use rupg_common::Result;
use rupg_types::{
    QueryItem, TSQUERY_AND, TSQUERY_NOT, TSQUERY_OR, TSQUERY_PHRASE, Value, legacy_crc32,
    ts_compare_string,
};

use crate::{Call, Kernel, bad_value};

/// `QTNode`: an item of a query with its operands as children. An operator of `&` or `|` can have more than two children.
#[derive(Clone, Debug)]
struct Node {
    item: QueryItem,
    /// `valcrc` of an operand, 0 for an operator.
    crc: i32,
    /// The bits of the operands below the node, one bit for each value of `crc` modulo 32.
    sign: u32,
    /// `QTN_NOCHANGE`: the node is a copy of the substitute.
    fixed: bool,
    children: Vec<Node>,
}

impl Node {
    /// `QT2QTN`: the tree of the item at `at`. The first child is the right operand.
    fn build(items: &[QueryItem], at: usize) -> Node {
        match &items[at] {
            QueryItem::Operand { word, .. } => {
                let crc = legacy_crc32(word.as_bytes());
                Node {
                    item: items[at].clone(),
                    crc,
                    sign: 1 << (crc.cast_unsigned() % 32),
                    fixed: false,
                    children: Vec::new(),
                }
            }
            QueryItem::Operator { op, left, .. } => {
                let mut children = vec![Node::build(items, at + 1)];
                if *op != TSQUERY_NOT {
                    children.push(Node::build(items, at + *left as usize));
                }
                let sign = children.iter().fold(0, |sign, c| sign | c.sign);
                Node { item: items[at].clone(), crc: 0, sign, fixed: false, children }
            }
        }
    }

    /// The operator of the node, or `None` for an operand.
    fn op(&self) -> Option<u8> {
        match self.item {
            QueryItem::Operator { op, .. } => Some(op),
            QueryItem::Operand { .. } => None,
        }
    }

    /// `QTNSort`: puts the children of each operator except `<N>` in the order of [`compare`].
    fn sort(&mut self) {
        let Some(op) = self.op() else { return };
        for child in &mut self.children {
            child.sort();
        }
        if self.children.len() > 1 && op != TSQUERY_PHRASE {
            let Ok(()) = rupg_common::qsort::<_, Infallible, _>(&mut self.children, &mut |a, b| {
                Ok(compare(a, b))
            });
        }
    }

    /// `QTNTernary`: moves the children of a child with the same operator `&` or `|` up into the node.
    fn ternary(&mut self) {
        let Some(op) = self.op() else { return };
        for child in &mut self.children {
            child.ternary();
        }
        if op != TSQUERY_AND && op != TSQUERY_OR {
            return;
        }
        let children = std::mem::take(&mut self.children);
        for child in children {
            if child.op() == Some(op) {
                self.children.extend(child.children);
            } else {
                self.children.push(child);
            }
        }
    }

    /// `QTNBinary`: puts a new operator over the first two children until each operator has two children or fewer.
    fn binary(&mut self) {
        let Some(op) = self.op() else { return };
        for child in &mut self.children {
            child.binary();
        }
        while self.children.len() > 2 {
            let mut rest = std::mem::take(&mut self.children).into_iter();
            let pair: Vec<Node> = rest.by_ref().take(2).collect();
            let mut rest: Vec<Node> = rest.collect();
            let last = rest.pop();
            let sign = pair[0].sign | pair[1].sign;
            let item = QueryItem::Operator { op, distance: 0, left: 0 };
            self.children = vec![Node { item, crc: 0, sign, fixed: false, children: pair }];
            self.children.extend(last);
            self.children.extend(rest);
        }
    }

    /// `fillQT`: the items of the tree, with `left` of each operator set again.
    fn fill(&self, out: &mut Vec<QueryItem>) {
        let at = out.len();
        out.push(self.item.clone());
        if let Some(first) = self.children.first() {
            first.fill(out);
        }
        if let Some(second) = self.children.get(1) {
            let offset = u32::try_from(out.len() - at).unwrap_or(u32::MAX);
            if let QueryItem::Operator { left, .. } = &mut out[at] {
                *left = offset;
            }
            second.fill(out);
        }
    }
}

/// `QTNodeCompare`: an order of the trees. Operators come before operands, then the operator with the greater code, the node with more children, the children in turn and the greater distance of `<N>`. Operands go by `valcrc` from the greatest and then by the bytes of the words.
fn compare(a: &Node, b: &Node) -> Ordering {
    match (&a.item, &b.item) {
        (QueryItem::Operator { .. }, QueryItem::Operand { .. }) => Ordering::Less,
        (QueryItem::Operand { .. }, QueryItem::Operator { .. }) => Ordering::Greater,
        (
            QueryItem::Operator { op: ao, distance: ad, .. },
            QueryItem::Operator { op: bo, distance: bd, .. },
        ) => {
            if ao != bo {
                return bo.cmp(ao);
            }
            if a.children.len() != b.children.len() {
                return b.children.len().cmp(&a.children.len());
            }
            for (x, y) in a.children.iter().zip(&b.children) {
                let order = compare(x, y);
                if order.is_ne() {
                    return order;
                }
            }
            if *ao == TSQUERY_PHRASE && ad != bd {
                return bd.cmp(ad);
            }
            Ordering::Equal
        }
        (QueryItem::Operand { word: aw, .. }, QueryItem::Operand { word: bw, .. }) => {
            if a.crc != b.crc {
                return b.crc.cmp(&a.crc);
            }
            ts_compare_string(aw.as_bytes(), bw.as_bytes()).cmp(&0)
        }
    }
}

/// `QTNEq`: two trees with the same operands that [`compare`] finds equal.
fn equal(a: &Node, b: &Node) -> bool {
    a.sign == b.sign && compare(a, b).is_eq()
}

/// A copy of the substitute, which the search does not go into again.
fn substitute(subs: Option<&Node>) -> Option<Node> {
    subs.map(|subs| Node { fixed: true, ..subs.clone() })
}

/// `findeq`: the substitute when the node is equal to `ex`, the node with the substitute in place of the children that match the children of `ex` for `&` and `|`, or else the node.
fn find_equal(mut node: Node, ex: &Node, subs: Option<&Node>) -> Option<Node> {
    if node.sign & ex.sign != ex.sign || node.op().is_some() != ex.op().is_some() || node.fixed {
        return Some(node);
    }
    if node.op().is_none() {
        if node.crc == ex.crc && equal(&node, ex) {
            return substitute(subs);
        }
        return Some(node);
    }
    if node.op() != ex.op() {
        return Some(node);
    }
    let (n, m) = (node.children.len(), ex.children.len());
    if n == m {
        if equal(&node, ex) {
            return substitute(subs);
        }
    } else if n > m && m > 0 {
        // The children of both nodes are in order, so one pass finds the matches.
        let mut matched = vec![false; n];
        let (mut i, mut j, mut count) = (0, 0, 0);
        while i < n && j < m {
            match compare(&node.children[i], &ex.children[j]) {
                Ordering::Equal => {
                    matched[i] = true;
                    count += 1;
                    i += 1;
                    j += 1;
                }
                Ordering::Less => i += 1,
                Ordering::Greater => break,
            }
        }
        if count == m {
            let children = std::mem::take(&mut node.children);
            node.children =
                children.into_iter().zip(matched).filter(|(_, hit)| !hit).map(|(c, _)| c).collect();
            node.children.extend(substitute(subs));
            node.sort();
        }
    }
    Some(node)
}

/// `dofindsubquery`: puts the substitute in place of `ex` at the node or else below it. An operator with no children left goes away, and an operator other than `!` with one child left becomes the child.
fn find(root: Node, ex: &Node, subs: Option<&Node>) -> Option<Node> {
    let mut root = find_equal(root, ex, subs)?;
    if root.fixed || root.op().is_none() {
        return Some(root);
    }
    let children = std::mem::take(&mut root.children);
    root.children = children.into_iter().filter_map(|c| find(c, ex, subs)).collect();
    match root.children.len() {
        0 => None,
        1 if root.op() != Some(TSQUERY_NOT) => root.children.pop(),
        _ => Some(root),
    }
}

/// The tree of a query in the form that the search compares: flat for `&` and `|`, and in order.
fn canonical(items: &[QueryItem]) -> Node {
    let mut tree = Node::build(items, 0);
    tree.ternary();
    tree.sort();
    tree
}

/// `tsquery_rewrite`: `ts_rewrite(query, target, substitute)`.
fn rewrite(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::TsQuery(query), Value::TsQuery(ex), Value::TsQuery(subst)] = args else {
        return Err(bad_value());
    };
    if query.is_empty() || ex.is_empty() {
        return Ok(Value::TsQuery(query.clone()));
    }
    let tree = canonical(query);
    let ex = canonical(ex);
    let subs = (!subst.is_empty()).then(|| Node::build(subst, 0));
    let Some(mut tree) = find(tree, &ex, subs.as_ref()) else {
        return Ok(Value::TsQuery(Vec::new()));
    };
    tree.binary();
    let mut out = Vec::with_capacity(query.len());
    tree.fill(&mut out);
    Ok(Value::TsQuery(out))
}

/// The kernel of `ts_rewrite` by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "tsquery_rewrite" => rewrite,
        _ => return None,
    })
}
