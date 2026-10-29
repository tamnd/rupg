//! `nodeToString` of PostgreSQL for the query of a view, which `pg_rewrite.ev_action` holds. The length of the text decides whether the value moves to the TOAST table and takes an OID, so the text must be the same as the text of PostgreSQL, byte for byte. The writer knows only a part of the nodes. For a query with other nodes, it gives `None`.

use std::collections::BTreeSet;
use std::fmt::Write;

use rupg_catalog::Catalog;
use rupg_types::{Numeric, NumericSign, Value};

use crate::collate::{self, State};
use crate::expr::{
    BoolOp, BoolTest, CastForm, Expr, ExprKind, FuncForm, NullTreatment, SqlValue, Var,
};
use crate::from::{FromItem, Join, JoinKind};
use crate::perm::PermInfo;
use crate::select::Query;
use crate::sort::SortGroup;
use crate::window::WindowClause;
use crate::{typcache, types};

/// `-FirstLowInvalidHeapAttributeNumber`: the offset of the column numbers in `selectedCols`.
const ATTNUM_OFFSET: i32 = 7;

/// `ev_action` of the `_RETURN` rule of a view: the query in a list, with the permission entries of the query in `perms` and the column names of the view in `aliases`, or `None` when the query has a node that the writer does not know.
pub(crate) fn view_action(
    query: &Query,
    perms: &[PermInfo],
    aliases: &[&str],
    catalog: Option<&Catalog>,
) -> Option<String> {
    let mut writer = Writer::new(query, perms, catalog)?;
    writer.out.push('(');
    writer.query(aliases)?;
    writer.out.push(')');
    Some(writer.out)
}

/// An entry of the range table.
enum Entry<'a> {
    Relation(usize),
    Join(&'a Join, usize, Vec<usize>),
    Group,
}

struct Writer<'a> {
    query: &'a Query,
    perms: &'a [PermInfo],
    catalog: Option<&'a Catalog>,
    out: String,
    /// The range table index of each relation of the query.
    rt: Vec<usize>,
    /// The entries of the range table, in order.
    entries: Vec<Entry<'a>>,
    /// For each relation, the range table indexes of the outer joins that can make its columns null.
    nulled: Vec<Vec<usize>>,
    /// The outer joins that can make the columns of the expression null, or `None` for all of them.
    scope: Option<Vec<usize>>,
    /// Each merged column of `USING` that is not a column of one side, as `COALESCE` of a full join or a cast: the value, the range table index of the join, the number of the column and a relation of the join. A reference to the column is a `Var` of the join.
    merged: Vec<(Expr, usize, usize, usize)>,
    /// Only the merged columns of the joins below this range table index change to a `Var` of the join.
    merged_below: usize,
    /// The range table index of the `*GROUP*` entry, or 0.
    group_rt: usize,
    /// True when the grouped expressions change to the columns of the `*GROUP*` entry.
    substitute: bool,
    /// `ressortgroupref` of each target.
    refs: Vec<usize>,
}

impl<'a> Writer<'a> {
    fn new(
        query: &'a Query,
        perms: &'a [PermInfo],
        catalog: Option<&'a Catalog>,
    ) -> Option<Writer<'a>> {
        let unsupported = query.set_op.is_some()
            || !query.ctes.is_empty()
            || query.windows.iter().any(|w| !w.run.is_empty())
            || !query.row_marks.is_empty()
            || !query.grouping_sets.is_empty()
            || query.group_distinct
            || query.distinct_on
            || query.write.is_some()
            || !query.params.is_empty()
            || query.notify.is_some()
            || query.is_return
            || query.pulled != 0
            || query.relations.iter().any(|r| {
                r.oid == 0
                    || r.subquery.is_some()
                    || r.function.is_some()
                    || r.values.is_some()
                    || r.cte.is_some()
                    || r.tablesample.is_some()
                    || r.excluded
            });
        if unsupported {
            return None;
        }
        let mut writer = Writer {
            query,
            perms,
            catalog,
            out: String::new(),
            rt: vec![0; query.relations.len()],
            entries: Vec::new(),
            nulled: vec![Vec::new(); query.relations.len()],
            scope: None,
            merged: Vec::new(),
            merged_below: usize::MAX,
            group_rt: 0,
            substitute: false,
            refs: vec![0; query.targets.len()],
        };
        for item in &query.from {
            writer.number(item)?;
        }
        if writer.rt.contains(&0) {
            return None;
        }
        if !query.group.is_empty() {
            writer.entries.push(Entry::Group);
            writer.group_rt = writer.entries.len();
        }
        // `assignSortGroupRef`: `ORDER BY` gets its numbers first, then `GROUP BY` and `DISTINCT`. The windows come last, and `transformWindowDefinitions` does the `ORDER BY` of a window before its `PARTITION BY`.
        let windows = query.windows.iter().flat_map(|w| w.order.iter().chain(&w.partition));
        let mut next = 0;
        for item in query.sort.iter().chain(&query.group).chain(&query.distinct).chain(windows) {
            if writer.refs[item.target] == 0 {
                next += 1;
                writer.refs[item.target] = next;
            }
        }
        Some(writer)
    }

    /// Gives the range table indexes to the relations of `item` and to its joins, and returns the relations in it.
    fn number(&mut self, item: &'a FromItem) -> Option<Vec<usize>> {
        match item {
            FromItem::Relation(i) => {
                if self.rt.get(*i)? != &0 {
                    return None;
                }
                self.entries.push(Entry::Relation(*i));
                self.rt[*i] = self.entries.len();
                Some(vec![*i])
            }
            FromItem::Join(join) => {
                if join.alias.is_some() || join.using_alias.is_some() {
                    return None;
                }
                let left = self.number(&join.left)?;
                let right = self.number(&join.right)?;
                let inner: Vec<usize> = left
                    .iter()
                    .chain(&right)
                    .flat_map(|&r| self.nulled[r].iter().copied())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                self.entries.push(Entry::Join(join, 0, inner));
                let index = self.entries.len();
                if let Some(Entry::Join(_, at, _)) = self.entries.last_mut() {
                    *at = index;
                }
                let nullable: &[usize] = match join.kind {
                    JoinKind::Inner => &[],
                    JoinKind::Left => &right,
                    JoinKind::Right => &left,
                    JoinKind::Full => &[left.as_slice(), right.as_slice()].concat(),
                    _ => return None,
                };
                for &r in nullable {
                    self.nulled[r].push(index);
                }
                for (at, m) in join.merged.iter().enumerate() {
                    if !matches!(m.kind, ExprKind::Var(_)) {
                        self.merged.push((unplaced(m), index, at + 1, left[0]));
                    }
                }
                Some([left, right].concat())
            }
        }
    }

    fn query(&mut self, aliases: &[&str]) -> Option<()> {
        let q = self.query;
        let has_aggs = q.targets.iter().map(|t| &t.expr).chain(&q.having).any(has_agg);
        let has_windows = q.targets.iter().any(|t| has_window(&t.expr));
        if q.exprs().into_iter().any(unsupported) {
            return None;
        }
        self.out.push_str("{QUERY :commandType 1 :querySource 0 :canSetTag true :utilityStmt <> :resultRelation 0");
        let _ = write!(
            self.out,
            " :hasAggs {} :hasWindowFuncs {} :hasTargetSRFs false :hasSubLinks false :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :hasGroupRTE {} :isReturn false :cteList <> :rtable ",
            has_aggs,
            has_windows,
            self.group_rt != 0
        );
        let entries: Vec<usize> = (0..self.entries.len()).collect();
        self.list(&entries, |w, &at| w.entry(at))?;
        self.out.push_str(" :rteperminfos ");
        self.perms()?;
        self.out.push_str(" :jointree {FROMEXPR :fromlist ");
        self.list(&q.from, |w, item| w.jointree_item(item))?;
        self.out.push_str(" :quals ");
        self.option(q.filter.as_ref())?;
        self.out.push_str(
            "} :mergeActionList <> :mergeTargetRelation 0 :mergeJoinCondition <> :targetList ",
        );
        self.substitute = self.group_rt != 0;
        let mut names = aliases.iter();
        let targets: Vec<_> = q.targets.iter().enumerate().collect();
        self.list(&targets, |w, &(i, t)| {
            w.out.push_str("{TARGETENTRY :expr ");
            w.expr(&t.expr)?;
            let name = if t.junk { None } else { Some(names.next().copied().unwrap_or(&t.name)) };
            let (table, column) = t.origin.unwrap_or((0, 0));
            let _ = write!(w.out, " :resno {} :resname ", i + 1);
            w.token(name);
            let _ = write!(
                w.out,
                " :ressortgroupref {} :resorigtbl {table} :resorigcol {column} :resjunk {}}}",
                w.refs[i], t.junk
            );
            Some(())
        })?;
        self.substitute = false;
        self.out.push_str(" :override 0 :onConflict <> :returningOldAlias <> :returningNewAlias <> :returningList <> :groupClause ");
        self.sort_groups(&q.group)?;
        self.out.push_str(" :groupDistinct false :groupingSets <> :havingQual ");
        self.substitute = self.group_rt != 0;
        self.option(q.having.as_ref())?;
        self.substitute = false;
        self.out.push_str(" :windowClause ");
        let windows: Vec<_> = q.windows.iter().enumerate().collect();
        self.list(&windows, |w, &(i, c)| w.window(i + 1, c))?;
        self.out.push_str(" :distinctClause ");
        self.sort_groups(&q.distinct)?;
        self.out.push_str(" :sortClause ");
        self.sort_groups(&q.sort)?;
        self.out.push_str(" :limitOffset ");
        self.option(q.offset.as_ref())?;
        self.out.push_str(" :limitCount ");
        self.option(q.limit.as_ref())?;
        let _ = write!(
            self.out,
            " :limitOption {} :rowMarks <> :setOperations <> :constraintDeps ",
            u8::from(q.with_ties)
        );
        if q.constraint_deps.is_empty() {
            self.out.push_str("<>");
        } else {
            self.out.push_str("(o");
            for oid in &q.constraint_deps {
                let _ = write!(self.out, " {oid}");
            }
            self.out.push(')');
        }
        self.out.push_str(" :withCheckOptions <> :stmt_location -1 :stmt_len -1}");
        Some(())
    }

    /// Writes the items as a list, or `<>` for no items.
    fn list<T>(
        &mut self,
        items: &[T],
        mut f: impl FnMut(&mut Self, &T) -> Option<()>,
    ) -> Option<()> {
        if items.is_empty() {
            self.out.push_str("<>");
            return Some(());
        }
        self.out.push('(');
        for (at, item) in items.iter().enumerate() {
            if at > 0 {
                self.out.push(' ');
            }
            f(self, item)?;
        }
        self.out.push(')');
        Some(())
    }

    fn option(&mut self, expr: Option<&Expr>) -> Option<()> {
        match expr {
            Some(e) => self.expr(e),
            None => {
                self.out.push_str("<>");
                Some(())
            }
        }
    }

    /// `outToken`, with `<>` for no text.
    fn token(&mut self, text: Option<&str>) {
        let Some(text) = text else {
            self.out.push_str("<>");
            return;
        };
        if text.is_empty() {
            self.out.push_str("\"\"");
            return;
        }
        let bytes = text.as_bytes();
        let sign = (bytes[0] == b'+' || bytes[0] == b'-')
            && bytes.get(1).is_some_and(|b| b.is_ascii_digit() || *b == b'.');
        if bytes[0] == b'<' || bytes[0].is_ascii_digit() || sign {
            self.out.push('\\');
        }
        for c in text.chars() {
            if matches!(c, ' ' | '\n' | '\t' | '(' | ')' | '{' | '}' | '\\') {
                self.out.push('\\');
            }
            self.out.push(c);
        }
    }

    /// A list of `String` nodes.
    fn names<'n>(&mut self, names: impl IntoIterator<Item = &'n str>) {
        let names: Vec<&str> = names.into_iter().collect();
        if names.is_empty() {
            self.out.push_str("<>");
            return;
        }
        self.out.push('(');
        for (at, name) in names.iter().enumerate() {
            if at > 0 {
                self.out.push(' ');
            }
            self.out.push('"');
            if !name.is_empty() {
                self.token(Some(name));
            }
            self.out.push('"');
        }
        self.out.push(')');
    }

    fn alias(&mut self, name: &str, columns: Option<Vec<&str>>) {
        self.out.push_str("{ALIAS :aliasname ");
        self.token(Some(name));
        self.out.push_str(" :colnames ");
        match columns {
            Some(columns) => self.names(columns),
            None => self.out.push_str("<>"),
        }
        self.out.push('}');
    }

    fn entry(&mut self, at: usize) -> Option<()> {
        self.out.push_str("{RANGETBLENTRY :alias ");
        match &self.entries[at] {
            Entry::Relation(i) => {
                let r = &self.query.relations[*i];
                match &r.alias {
                    Some(given) => {
                        self.alias(&r.name, Some(given.iter().map(String::as_str).collect()))
                    }
                    None => self.out.push_str("<>"),
                }
                self.out.push_str(" :eref ");
                let given = r.alias_names();
                let columns =
                    r.columns.iter().zip(given).map(|(c, a)| a.unwrap_or(&c.name)).collect();
                self.alias(&r.name, Some(columns));
                let kind = self.catalog?.relation(r.oid)?.kind.code();
                let perm = match r.perm {
                    Some(p) => p + 1,
                    None => 0,
                };
                let _ = write!(
                    self.out,
                    " :rtekind 0 :relid {} :inh {} :relkind {kind} :rellockmode 1 :perminfoindex {perm} :tablesample <> :lateral false :inFromCl true :securityQuals <>}}",
                    r.oid, !r.only
                );
            }
            Entry::Join(join, index, inner) => {
                let (join, index, inner) = (*join, *index, inner.clone());
                self.out.push_str("<> :eref ");
                self.alias(
                    "unnamed_join",
                    Some(join.columns.iter().map(|c| c.0.as_str()).collect()),
                );
                let _ = write!(
                    self.out,
                    " :rtekind 2 :jointype {} :joinmergedcols {} :joinaliasvars ",
                    join_type(join.kind)?,
                    join.merged.len()
                );
                // The columns of `joinaliasvars` are the columns of the relations, which the join itself can make null.
                self.scope = Some([inner, vec![index]].concat());
                self.merged_below = index;
                let columns: Vec<&Expr> = join.columns.iter().map(|c| &c.1).collect();
                let written = self.list(&columns, |w, e| w.expr(e));
                self.merged_below = usize::MAX;
                self.scope = None;
                written?;
                let left: Vec<usize> =
                    join.sources.iter().filter(|s| s.0 != 0).map(|s| s.0).collect();
                let right: Vec<usize> =
                    join.sources.iter().filter(|s| s.1 != 0).map(|s| s.1).collect();
                self.out.push_str(" :joinleftcols ");
                self.ints(&left);
                self.out.push_str(" :joinrightcols ");
                self.ints(&right);
                self.out.push_str(
                    " :join_using_alias <> :lateral false :inFromCl true :securityQuals <>}",
                );
            }
            Entry::Group => {
                let q = self.query;
                self.out.push_str("<> :eref ");
                let names: Vec<&str> = q
                    .group
                    .iter()
                    .map(|g| {
                        let t = &q.targets[g.target];
                        if t.junk { "?column?" } else { t.name.as_str() }
                    })
                    .collect();
                self.alias("*GROUP*", Some(names));
                self.out.push_str(" :rtekind 9 :groupexprs ");
                self.list(&q.group, |w, g| w.expr(&q.targets[g.target].expr))?;
                self.out.push_str(" :lateral false :inFromCl false :securityQuals <>}");
            }
        }
        Some(())
    }

    /// An integer list.
    fn ints(&mut self, values: &[usize]) {
        if values.is_empty() {
            self.out.push_str("<>");
            return;
        }
        self.out.push_str("(i");
        for v in values {
            let _ = write!(self.out, " {v}");
        }
        self.out.push(')');
    }

    fn join_index(&self, join: &Join) -> Option<usize> {
        self.entries.iter().find_map(|e| match e {
            Entry::Join(j, index, _) if std::ptr::eq(*j, join) => Some(*index),
            _ => None,
        })
    }

    fn perms(&mut self) -> Option<()> {
        let (q, perms) = (self.query, self.perms);
        let used: Vec<usize> = q.relations.iter().filter_map(|r| r.perm).collect();
        if used.len() != perms.len() || (0..perms.len()).any(|p| !used.contains(&p)) {
            return None;
        }
        self.list(perms, |w, p| {
            let only = q.relations.iter().find(|r| r.oid == p.relid).is_some_and(|r| r.only);
            let _ = write!(
                w.out,
                "{{RTEPERMISSIONINFO :relid {} :inh {} :requiredPerms {} :checkAsUser 0 :selectedCols ",
                p.relid, !only, p.required
            );
            w.bits(p.selected.iter().map(|&a| i32::from(a) + ATTNUM_OFFSET));
            w.out.push_str(" :insertedCols ");
            w.bits(p.inserted.iter().map(|&a| i32::from(a) + ATTNUM_OFFSET));
            w.out.push_str(" :updatedCols ");
            w.bits(p.updated.iter().map(|&a| i32::from(a) + ATTNUM_OFFSET));
            w.out.push('}');
            Some(())
        })
    }

    fn bits(&mut self, members: impl IntoIterator<Item = i32>) {
        self.out.push_str("(b");
        for m in members.into_iter().collect::<BTreeSet<_>>() {
            let _ = write!(self.out, " {m}");
        }
        self.out.push(')');
    }

    fn jointree_item(&mut self, item: &FromItem) -> Option<()> {
        match item {
            FromItem::Relation(i) => {
                let _ = write!(self.out, "{{RANGETBLREF :rtindex {}}}", self.rt[*i]);
            }
            FromItem::Join(join) => {
                let index = self.join_index(join)?;
                let _ = write!(
                    self.out,
                    "{{JOINEXPR :jointype {} :isNatural {} :larg ",
                    join_type(join.kind)?,
                    join.natural
                );
                self.jointree_item(&join.left)?;
                self.out.push_str(" :rarg ");
                self.jointree_item(&join.right)?;
                self.out.push_str(" :usingClause ");
                self.names(join.using.iter().map(String::as_str));
                self.out.push_str(" :join_using_alias <> :quals ");
                let inner = self.entries.iter().find_map(|e| match e {
                    Entry::Join(_, at, inner) if *at == index => Some(inner.clone()),
                    _ => None,
                });
                self.scope = inner;
                let written = self.option(join.on.as_ref());
                self.scope = None;
                written?;
                let _ = write!(self.out, " :alias <> :rtindex {index}}}");
            }
        }
        Some(())
    }

    fn sort_groups(&mut self, items: &[SortGroup]) -> Option<()> {
        let q = self.query;
        self.list(items, |w, s| {
            let (eq, reverse) = if s.operator == 0 {
                (typcache::operators(q.targets[s.target].expr.ty).eq, false)
            } else {
                typcache::equality_for_ordering(s.operator)?
            };
            let _ = write!(
                w.out,
                "{{SORTGROUPCLAUSE :tleSortGroupRef {} :eqop {eq} :sortop {} :reverse_sort {reverse} :nulls_first {} :hashable {}}}",
                w.refs[s.target], s.operator, s.nulls_first, s.hashable
            );
            Some(())
        })
    }

    fn window(&mut self, winref: usize, c: &WindowClause) -> Option<()> {
        self.out.push_str("{WINDOWCLAUSE :name ");
        self.token(c.name.as_deref());
        self.out.push_str(" :refname ");
        self.token(c.refname.as_deref());
        self.out.push_str(" :partitionClause ");
        self.sort_groups(&c.partition)?;
        self.out.push_str(" :orderClause ");
        self.sort_groups(&c.order)?;
        let f = &c.frame;
        let _ = write!(self.out, " :frameOptions {} :startOffset ", f.options);
        self.option(f.start.as_ref())?;
        self.out.push_str(" :endOffset ");
        self.option(f.end.as_ref())?;
        // `inRangeColl` is the collation of the column of `ORDER BY`, only in `RANGE` mode with an offset.
        let in_range_collation = match c.order.first() {
            Some(s) if f.start_in_range != 0 || f.end_in_range != 0 => {
                self.collation(&self.query.targets[s.target].expr)
            }
            _ => 0,
        };
        let _ = write!(
            self.out,
            " :startInRangeFunc {} :endInRangeFunc {} :inRangeColl {in_range_collation} :inRangeAsc {} :inRangeNullsFirst {} :winref {winref} :copiedOrder {}}}",
            f.start_in_range,
            f.end_in_range,
            f.in_range_asc,
            f.in_range_nulls_first,
            c.copied_order
        );
        Some(())
    }

    /// `exprCollation`.
    fn collation(&self, e: &Expr) -> u32 {
        collate::expr_collation(e, &[&self.query.relations], self.catalog)
    }

    /// The collation that the inputs of a node give it, `inputcollid`.
    fn input_collation(&self, args: &[Expr]) -> u32 {
        let levels: &[&[crate::Relation]] = &[&self.query.relations];
        let mut merged = State::None;
        for arg in args {
            let Ok(state) = collate::state(arg, levels, self.catalog) else { return 0 };
            match merged.merge(state, self.catalog) {
                Ok(m) => merged = m,
                Err(_) => return 0,
            }
        }
        merged.oid()
    }

    fn exprs(&mut self, args: &[Expr]) -> Option<()> {
        self.list(args, |w, e| w.expr(e))
    }

    fn expr(&mut self, e: &Expr) -> Option<()> {
        if self.substitute
            && let Some(at) = self.grouped(e)
        {
            let g = &self.query.targets[self.query.group[at].target].expr;
            let collation = self.collation(g);
            let (index, number) = (self.group_rt, at + 1);
            let _ = write!(
                self.out,
                "{{VAR :varno {index} :varattno {number} :vartype {} :vartypmod {} :varcollid {collation} :varnullingrels (b) :varlevelsup 0 :varreturningtype 0 :varnosyn {index} :varattnosyn {number} :location -1}}",
                g.ty, g.typmod
            );
            return Some(());
        }
        if let Some(at) = self.merged_column(e) {
            return self.join_var(at, e);
        }
        match &e.kind {
            ExprKind::Var(var) => self.var(*var, e),
            ExprKind::Const(value) => self.constant(value, e),
            ExprKind::Func(f) => {
                if f.retset {
                    return None;
                }
                if let FuncForm::Operator(opno) = f.form {
                    self.op("OPEXPR", opno, f.oid, e, &f.args)
                } else {
                    let format = match f.form {
                        FuncForm::ExplicitCast => 1,
                        FuncForm::ImplicitCast => 2,
                        FuncForm::SqlSyntax => 3,
                        _ => 0,
                    };
                    let (collation, input) = (self.collation(e), self.input_collation(&f.args));
                    let _ = write!(
                        self.out,
                        "{{FUNCEXPR :funcid {} :funcresulttype {} :funcretset false :funcvariadic {} :funcformat {format} :funccollid {collation} :inputcollid {input} :args ",
                        f.oid, e.ty, f.variadic
                    );
                    self.exprs(&f.args)?;
                    self.out.push_str(" :location -1}");
                    Some(())
                }
            }
            ExprKind::Relabel(arg, form) => {
                self.out.push_str("{RELABELTYPE :arg ");
                self.expr(arg)?;
                let _ = write!(
                    self.out,
                    " :resulttype {} :resulttypmod {} :resultcollid {} :relabelformat {} :location -1}}",
                    e.ty,
                    e.typmod,
                    self.collation(e),
                    cast_format(*form)
                );
                Some(())
            }
            ExprKind::CoerceViaIo(arg, form) => {
                self.out.push_str("{COERCEVIAIO :arg ");
                self.expr(arg)?;
                let _ = write!(
                    self.out,
                    " :resulttype {} :resultcollid {} :coerceformat {} :location -1}}",
                    e.ty,
                    self.collation(e),
                    cast_format(*form)
                );
                Some(())
            }
            ExprKind::Bool(op, args) => {
                let name = match op {
                    BoolOp::And => "and",
                    BoolOp::Or => "or",
                    BoolOp::Not => "not",
                };
                let _ = write!(self.out, "{{BOOLEXPR :boolop {name} :args ");
                self.exprs(args)?;
                self.out.push_str(" :location -1}");
                Some(())
            }
            ExprKind::NullTest(arg, is_null) => {
                if types::is_complex(arg.ty) {
                    return None;
                }
                self.out.push_str("{NULLTEST :arg ");
                self.expr(arg)?;
                let _ = write!(
                    self.out,
                    " :nulltesttype {} :argisrow false :location -1}}",
                    u8::from(!is_null)
                );
                Some(())
            }
            ExprKind::BooleanTest(arg, test) => {
                self.out.push_str("{BOOLEANTEST :arg ");
                self.expr(arg)?;
                let test = match test {
                    BoolTest::IsTrue => 0,
                    BoolTest::IsNotTrue => 1,
                    BoolTest::IsFalse => 2,
                    BoolTest::IsNotFalse => 3,
                    BoolTest::IsUnknown => 4,
                    BoolTest::IsNotUnknown => 5,
                };
                let _ = write!(self.out, " :booltesttype {test} :location -1}}");
                Some(())
            }
            ExprKind::Case(case) if case.arg.is_none() => {
                let _ = write!(
                    self.out,
                    "{{CASEEXPR :casetype {} :casecollid {} :arg <> :args ",
                    e.ty,
                    self.collation(e)
                );
                self.list(&case.whens, |w, (when, then)| {
                    w.out.push_str("{CASEWHEN :expr ");
                    w.expr(when)?;
                    w.out.push_str(" :result ");
                    w.expr(then)?;
                    w.out.push_str(" :location -1}");
                    Some(())
                })?;
                self.out.push_str(" :defresult ");
                self.expr(&case.default)?;
                self.out.push_str(" :location -1}");
                Some(())
            }
            ExprKind::Coalesce(args) => {
                let _ = write!(
                    self.out,
                    "{{COALESCEEXPR :coalescetype {} :coalescecollid {} :args ",
                    e.ty,
                    self.collation(e)
                );
                self.exprs(args)?;
                self.out.push_str(" :location -1}");
                Some(())
            }
            ExprKind::Collate(arg, collation) => {
                self.out.push_str("{COLLATEEXPR :arg ");
                self.expr(arg)?;
                let _ = write!(self.out, " :collOid {collation} :location -1}}");
                Some(())
            }
            ExprKind::Agg(agg) => {
                if agg.levels_up != 0
                    || agg.kind != b'n'
                    || !agg.direct.is_empty()
                    || !agg.order.is_empty()
                    || !agg.distinct.is_empty()
                {
                    return None;
                }
                let (collation, input) = (self.collation(e), self.input_collation(&agg.args));
                let _ = write!(
                    self.out,
                    "{{AGGREF :aggfnoid {} :aggtype {} :aggcollid {collation} :inputcollid {input} :aggtranstype 0 :aggargtypes ",
                    agg.oid, e.ty
                );
                if agg.args.is_empty() {
                    self.out.push_str("<>");
                } else {
                    self.out.push_str("(o");
                    for arg in &agg.args {
                        let _ = write!(self.out, " {}", arg.ty);
                    }
                    self.out.push(')');
                }
                self.out.push_str(" :aggdirectargs <> :args ");
                let substitute = std::mem::replace(&mut self.substitute, false);
                let args: Vec<_> = agg.args.iter().enumerate().collect();
                let written = self.list(&args, |w, &(i, arg)| {
                    w.out.push_str("{TARGETENTRY :expr ");
                    w.expr(arg)?;
                    let _ = write!(
                        w.out,
                        " :resno {} :resname <> :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}}",
                        i + 1
                    );
                    Some(())
                });
                written?;
                self.out.push_str(" :aggorder <> :aggdistinct <> :aggfilter ");
                let written = self.option(agg.filter.as_deref());
                self.substitute = substitute;
                written?;
                let _ = write!(
                    self.out,
                    " :aggstar {} :aggvariadic {} :aggkind n :aggpresorted false :agglevelsup 0 :aggsplit 0 :aggno -1 :aggtransno -1 :location -1}}",
                    agg.star, agg.variadic
                );
                Some(())
            }
            ExprKind::Window(f) => {
                let (collation, input) = (self.collation(e), self.input_collation(&f.args));
                let _ = write!(
                    self.out,
                    "{{WINDOWFUNC :winfnoid {} :wintype {} :wincollid {collation} :inputcollid {input} :args ",
                    f.oid, e.ty
                );
                self.exprs(&f.args)?;
                self.out.push_str(" :aggfilter ");
                self.option(f.filter.as_deref())?;
                // `PARSER_IGNORE_NULLS` and `PARSER_RESPECT_NULLS`.
                let nulls = match f.nulls {
                    NullTreatment::None => 0,
                    NullTreatment::Ignore => 1,
                    NullTreatment::Respect => 2,
                };
                let _ = write!(
                    self.out,
                    " :runCondition <> :winref {} :winstar {} :winagg {} :ignore_nulls {nulls} :location -1}}",
                    f.winref, f.star, f.agg
                );
                Some(())
            }
            ExprKind::SqlValue(value) => {
                let op = match value {
                    SqlValue::CurrentDate => 0,
                    SqlValue::CurrentTime(p) => 1 + u8::from(p.is_some()),
                    SqlValue::CurrentTimestamp(p) => 3 + u8::from(p.is_some()),
                    SqlValue::LocalTime(p) => 5 + u8::from(p.is_some()),
                    SqlValue::LocalTimestamp(p) => 7 + u8::from(p.is_some()),
                    SqlValue::CurrentRole => 9,
                    SqlValue::CurrentUser => 10,
                    SqlValue::User => 11,
                    SqlValue::SessionUser => 12,
                    SqlValue::CurrentCatalog => 13,
                    SqlValue::CurrentSchema => 14,
                };
                let _ = write!(
                    self.out,
                    "{{SQLVALUEFUNCTION :op {op} :type {} :typmod {} :location -1}}",
                    e.ty, e.typmod
                );
                Some(())
            }
            _ => None,
        }
    }

    /// `OpExpr` and the nodes of the same form.
    fn op(&mut self, node: &str, opno: u32, func: u32, e: &Expr, args: &[Expr]) -> Option<()> {
        let (collation, input) = (self.collation(e), self.input_collation(args));
        let _ = write!(
            self.out,
            "{{{node} :opno {opno} :opfuncid {func} :opresulttype {} :opretset false :opcollid {collation} :inputcollid {input} :args ",
            e.ty
        );
        self.exprs(args)?;
        self.out.push_str(" :location -1}");
        Some(())
    }

    /// The merged column of a join that `e` is, as the number in `merged`.
    fn merged_column(&self, e: &Expr) -> Option<usize> {
        if self.merged.is_empty() || matches!(e.kind, ExprKind::Var(_) | ExprKind::Const(_)) {
            return None;
        }
        let e = unplaced(e);
        self.merged.iter().position(|(m, index, _, _)| *index < self.merged_below && *m == e)
    }

    /// A `Var` of a merged column of a join. Only the outer joins above the join can make it null.
    fn join_var(&mut self, at: usize, e: &Expr) -> Option<()> {
        let (index, number, relation) = (self.merged[at].1, self.merged[at].2, self.merged[at].3);
        let collation = self.collation(e);
        let _ = write!(
            self.out,
            "{{VAR :varno {index} :varattno {number} :vartype {} :vartypmod {} :varcollid {collation} :varnullingrels ",
            e.ty, e.typmod
        );
        let nulled: Vec<i32> = self.nulled[relation]
            .iter()
            .filter(|&&j| j > index && self.scope.as_ref().is_none_or(|s| s.contains(&j)))
            .map(|&j| i32::try_from(j).unwrap_or(0))
            .collect();
        self.bits(nulled);
        let _ = write!(
            self.out,
            " :varlevelsup 0 :varreturningtype 0 :varnosyn {index} :varattnosyn {number} :location -1}}"
        );
        Some(())
    }

    fn var(&mut self, var: Var, e: &Expr) -> Option<()> {
        if var.levels_up != 0 {
            return None;
        }
        let index = *self.rt.get(var.relation)?;
        let collation = self.collation(e);
        let _ = write!(
            self.out,
            "{{VAR :varno {index} :varattno {} :vartype {} :vartypmod {} :varcollid {collation} :varnullingrels ",
            var.attnum, e.ty, e.typmod
        );
        let nulled: Vec<i32> = self.nulled[var.relation]
            .iter()
            .filter(|&j| self.scope.as_ref().is_none_or(|s| s.contains(j)))
            .map(|&j| i32::try_from(j).unwrap_or(0))
            .collect();
        self.bits(nulled);
        let _ = write!(
            self.out,
            " :varlevelsup 0 :varreturningtype 0 :varnosyn {index} :varattnosyn {} :location -1}}",
            var.attnum
        );
        Some(())
    }

    fn constant(&mut self, value: &Value, e: &Expr) -> Option<()> {
        let row = types::row(e.ty)?;
        let collation = self.collation(e);
        let _ = write!(
            self.out,
            "{{CONST :consttype {} :consttypmod {} :constcollid {collation} :constlen {} :constbyval {} :constisnull {} :location -1 :constvalue ",
            e.ty,
            e.typmod,
            row.len,
            row.by_val,
            matches!(value, Value::Null)
        );
        let bytes: Vec<u8> = match value {
            Value::Null => {
                self.out.push_str("<>}");
                return Some(());
            }
            Value::Bool(b) => i64::from(*b).to_le_bytes().to_vec(),
            Value::Int2(v) => i64::from(*v).to_le_bytes().to_vec(),
            Value::Int4(v) | Value::Date(v) => i64::from(*v).to_le_bytes().to_vec(),
            Value::Int8(v) | Value::Timestamp(v) | Value::TimestampTz(v) | Value::Time(v) => {
                v.to_le_bytes().to_vec()
            }
            Value::Oid(v) => u64::from(*v).to_le_bytes().to_vec(),
            Value::Float4(v) => i64::from(v.to_bits().cast_signed()).to_le_bytes().to_vec(),
            Value::Float8(v) => v.to_bits().to_le_bytes().to_vec(),
            Value::Char(c) => i64::from(c.cast_signed()).to_le_bytes().to_vec(),
            Value::Text(s) if row.len == -1 && !row.by_val => {
                let len = u32::try_from(s.len() + 4).ok()?;
                let mut bytes = (len << 2).to_le_bytes().to_vec();
                bytes.extend_from_slice(s.as_bytes());
                bytes
            }
            Value::Numeric(n) => numeric(n)?,
            Value::Interval(v) => {
                let mut bytes = v.time.to_le_bytes().to_vec();
                bytes.extend_from_slice(&v.day.to_le_bytes());
                bytes.extend_from_slice(&v.month.to_le_bytes());
                bytes
            }
            Value::Text(s) if row.len == 64 => {
                let mut bytes = s.as_bytes().to_vec();
                bytes.resize(64, 0);
                bytes
            }
            _ => return None,
        };
        let _ = write!(
            self.out,
            "{} [ ",
            if row.by_val { usize::try_from(row.len).ok()? } else { bytes.len() }
        );
        for b in bytes {
            let _ = write!(self.out, "{} ", b.cast_signed());
        }
        self.out.push_str("]}");
        Some(())
    }

    /// The number from 0 of the item of `GROUP BY` that is the same as `e`.
    fn grouped(&self, e: &Expr) -> Option<usize> {
        if matches!(e.kind, ExprKind::Const(_))
            && !self
                .query
                .group
                .iter()
                .any(|g| matches!(self.query.targets[g.target].expr.kind, ExprKind::Const(_)))
        {
            return None;
        }
        let e = unplaced(e);
        self.query.group.iter().position(|g| unplaced(&self.query.targets[g.target].expr) == e)
    }
}

/// The bytes of a `numeric` value as `make_result` packs them: the short form when the display scale and the weight fit in it.
fn numeric(n: &Numeric) -> Option<Vec<u8>> {
    let special = match n.sign() {
        NumericSign::NaN => Some(0xc000u16),
        NumericSign::Infinity => Some(0xd000),
        NumericSign::NegativeInfinity => Some(0xf000),
        _ => None,
    };
    let mut words: Vec<u16> = Vec::new();
    if let Some(code) = special {
        words.push(code);
    } else {
        let digits = n.digits();
        let lead = digits.iter().take_while(|&&d| d == 0).count();
        let digits = &digits[lead..];
        let tail = digits.iter().rev().take_while(|&&d| d == 0).count();
        let digits = &digits[..digits.len() - tail];
        let lead = i16::try_from(lead).ok()?;
        let (negative, weight) = if digits.is_empty() {
            (false, 0)
        } else {
            (n.sign() == NumericSign::Negative, n.weight() - lead)
        };
        let dscale = n.dscale();
        if dscale <= 63 && (-64..=63).contains(&weight) {
            let mut header = 0x8000 | (dscale << 7) | (weight.cast_unsigned() & 0x3f);
            if negative {
                header |= 0x2000;
            }
            if weight < 0 {
                header |= 0x0040;
            }
            words.push(header);
        } else {
            words.push(if negative { 0x4000 } else { 0 } | dscale);
            words.push(weight.cast_unsigned());
        }
        words.extend(digits.iter().map(|d| d.cast_unsigned()));
    }
    let len = u32::try_from(4 + 2 * words.len()).ok()?;
    let mut bytes = (len << 2).to_le_bytes().to_vec();
    for word in words {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    Some(bytes)
}

/// The expression with no places, as `equal` ignores the locations.
fn unplaced(e: &Expr) -> Expr {
    let mut e = e.clone();
    fn clear(e: &mut Expr) {
        e.location = None;
        for child in e.children_mut() {
            clear(child);
        }
    }
    clear(&mut e);
    e
}

fn has_window(e: &Expr) -> bool {
    matches!(e.kind, ExprKind::Window(_)) || e.children().into_iter().any(has_window)
}

fn has_agg(e: &Expr) -> bool {
    matches!(e.kind, ExprKind::Agg(_)) || e.children().into_iter().any(has_agg)
}

/// True when the expression has a subquery or a `GROUPING` call, which the writer does not write.
fn unsupported(e: &Expr) -> bool {
    matches!(e.kind, ExprKind::SubLink(_) | ExprKind::Grouping(_))
        || e.children().into_iter().any(unsupported)
}

fn join_type(kind: JoinKind) -> Option<u8> {
    Some(match kind {
        JoinKind::Inner => 0,
        JoinKind::Left => 1,
        JoinKind::Full => 2,
        JoinKind::Right => 3,
        _ => return None,
    })
}

fn cast_format(form: CastForm) -> u8 {
    match form {
        CastForm::Explicit => 1,
        CastForm::Implicit => 2,
    }
}
