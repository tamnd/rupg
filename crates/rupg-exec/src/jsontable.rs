//! `JSON_TABLE` in `FROM`: the plan states of `jsonpath_exec.c` that give the rows, and `JsonTableGetValue` for the columns.

use rupg_analyze::{JsonTable, JsonTablePlan, JsonTableScan};
use rupg_common::Result;
use rupg_func::{Call, PathVar, SqlJsonItem};
use rupg_types::{Value, oid};

use crate::{Eval, Rows};

/// `JsonTablePlanState`.
enum State<'p> {
    Scan(Box<Scan<'p>>),
    Sibling(Box<State<'p>>, Box<State<'p>>),
}

/// The state of a `JsonTablePathScan`.
struct Scan<'p> {
    plan: &'p JsonTableScan,
    /// The items that the path found and that the scan did not give yet.
    items: std::vec::IntoIter<SqlJsonItem>,
    /// The current row as the text of `jsonb`, or `None` when the scan has no row.
    current: Option<Value>,
    ordinal: i32,
    nested: Option<State<'p>>,
}

/// The values that each path of the plan uses.
struct Paths<'c> {
    call: Call<'c>,
    vars: Vec<PathVar>,
}

impl<'p> State<'p> {
    /// `JsonTableInitPlan`.
    fn new(plan: &'p JsonTablePlan) -> State<'p> {
        match plan {
            JsonTablePlan::Scan(scan) => State::Scan(Box::new(Scan {
                plan: scan,
                items: Vec::new().into_iter(),
                current: None,
                ordinal: 0,
                nested: scan.child.as_ref().map(State::new),
            })),
            JsonTablePlan::Sibling(left, right) => {
                State::Sibling(Box::new(State::new(left)), Box::new(State::new(right)))
            }
        }
    }

    /// `JsonTablePlanNextRow`.
    fn next_row(&mut self, paths: &Paths<'_>) -> Result<bool> {
        match self {
            State::Scan(scan) => scan.next_row(paths),
            State::Sibling(left, right) => Ok(left.next_row(paths)? || right.next_row(paths)?),
        }
    }

    /// `JsonTableResetNestedPlan`: the paths of the nested plans on the new row of the parent.
    fn reset_nested(&mut self, paths: &Paths<'_>, parent: &Value) -> Result<()> {
        match self {
            State::Scan(scan) => scan.reset(paths, parent),
            State::Sibling(left, right) => {
                left.reset_nested(paths, parent)?;
                right.reset_nested(paths, parent)
            }
        }
    }

    /// The state of the scan that has the column.
    fn scan_of(&self, column: usize) -> Option<&Scan<'p>> {
        match self {
            State::Scan(scan) if scan.plan.columns.contains(&column) => Some(scan),
            State::Scan(scan) => scan.nested.as_ref().and_then(|n| n.scan_of(column)),
            State::Sibling(left, right) => left.scan_of(column).or_else(|| right.scan_of(column)),
        }
    }
}

impl Scan<'_> {
    /// `JsonTableResetRowPattern`: the path of the scan on an item. A soft error gives no rows.
    fn reset(&mut self, paths: &Paths<'_>, item: &Value) -> Result<()> {
        let throw = self.plan.error_on_error;
        let found =
            rupg_func::sql_json_path(&paths.call, item, &self.plan.path, &paths.vars, throw, true)?;
        self.items = found.map(|(_, items)| items).unwrap_or_default().into_iter();
        self.current = None;
        self.ordinal = 0;
        Ok(())
    }

    /// `JsonTablePlanScanNextRow`.
    fn next_row(&mut self, paths: &Paths<'_>) -> Result<bool> {
        if self.current.is_some()
            && let Some(nested) = &mut self.nested
            && nested.next_row(paths)?
        {
            return Ok(true);
        }
        let Some(item) = self.items.next() else {
            self.current = None;
            return Ok(false);
        };
        let current = Value::Text(item.jsonb());
        self.ordinal += 1;
        if let Some(nested) = &mut self.nested {
            nested.reset_nested(paths, &current)?;
            nested.next_row(paths)?;
        }
        self.current = Some(current);
        Ok(true)
    }
}

impl Eval<'_> {
    /// `tfuncFetchRows` for `JSON_TABLE`: the values of `PASSING`, then the context item, then a row for each row of the plan. A null context item gives no rows.
    pub(crate) fn json_table(&mut self, table: &JsonTable) -> Result<Rows> {
        let mut vars = Vec::with_capacity(table.passing.len());
        for (name, arg) in &table.passing {
            vars.push(PathVar { name: name.clone(), ty: arg.ty, value: self.eval(arg)? });
        }
        let doc = self.eval(&table.context)?;
        if doc.is_null() {
            return Ok(Vec::new());
        }
        let call = Call { session: self.session, args: &[], ret: oid::JSONB, variadic: false };
        let paths = Paths { call, vars };
        let mut root = State::new(&table.plan);
        if let State::Scan(scan) = &mut root {
            scan.reset(&paths, &doc)?;
        }
        let mut rows = Vec::new();
        while root.next_row(&paths)? {
            let mut row = Vec::with_capacity(table.columns.len());
            for (at, column) in table.columns.iter().enumerate() {
                let Some(scan) = root.scan_of(at) else {
                    row.push(Value::Null);
                    continue;
                };
                let value = match (&scan.current, &column.expr) {
                    (None, _) => Value::Null,
                    (Some(current), Some(expr)) => {
                        self.case.push(current.clone());
                        let value = self.eval(expr);
                        self.case.pop();
                        value?
                    }
                    (Some(_), None) => Value::Int4(scan.ordinal),
                };
                row.push(value);
            }
            rows.push(row);
        }
        Ok(rows)
    }
}
