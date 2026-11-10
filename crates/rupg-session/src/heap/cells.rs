//! The rows of a bulk load as typed cells, which go straight into the encoded columns of a table with no `Value` for each field and no values in the versions (spec/10 section 10.8). The versions of the rows get their places in the pages as [`Heap::insert`] gives them, so each row has the same `ctid` as in PostgreSQL.

use std::sync::Arc;

use rupg_column::segment::{Cell, Kind, ROWS, Store};
use rupg_column::stored::Stored;
use rupg_common::{Error, Result};
use rupg_platform::Tasks;

use super::pages::{self, TOAST_THRESHOLD, Width, max_align, plain_lens};
use super::{Frozen, Heap, State, Version, View, on_runs};

/// The values of one column of the rows of a load.
#[derive(Debug)]
pub(crate) enum Values {
    /// The values of a column whose kind keeps integers.
    Ints(Vec<i64>),
    /// The bytes of the values one after the other, and the end of each value in the bytes.
    Bytes(Vec<u8>, Vec<usize>),
}

impl Values {
    /// No values, for a column of the kind `kind`.
    pub(crate) fn of(kind: Kind) -> Values {
        match kind.store() {
            Store::Code => Values::Bytes(Vec::new(), Vec::new()),
            Store::Int | Store::Float => Values::Ints(Vec::new()),
        }
    }

    /// Adds the values of `other` after the values.
    pub(crate) fn append(&mut self, other: &mut Values) {
        match (self, other) {
            (Values::Ints(values), Values::Ints(more)) => values.append(more),
            (Values::Bytes(bytes, ends), Values::Bytes(more, more_ends)) => {
                let base = bytes.len();
                bytes.append(more);
                ends.extend(more_ends.drain(..).map(|end| base + end));
            }
            _ => {}
        }
    }

    fn len(&self) -> usize {
        match self {
            Values::Ints(values) => values.len(),
            Values::Bytes(_, ends) => ends.len(),
        }
    }

    /// Puts the values of the rows from `from` to `to` on the rows of `stored`.
    fn extend(&self, stored: &mut Stored, from: usize, to: usize) -> Result<()> {
        match self {
            Values::Ints(values) => {
                stored.extend_cells(values[from..to].iter().map(|&value| Ok(Cell::Int(value))))
            }
            Values::Bytes(bytes, ends) => {
                let mut start = from.checked_sub(1).map_or(0, |last| ends[last]);
                stored.extend_cells(ends[from..to].iter().map(|&end| {
                    let cell = Cell::Bytes(&bytes[start..end]);
                    start = end;
                    Ok(cell)
                }))
            }
        }
    }
}

/// The rows of a load, a column at a time, with no nulls.
#[derive(Debug, Default)]
pub(crate) struct Cells {
    pub(crate) rows: usize,
    /// The kind and the values of each column of the table, in the order of the columns.
    pub(crate) columns: Vec<(Kind, Values)>,
}

/// `attlen` and `attalign` of a kind that keeps integers, as [`pages::layout`] gives them for the value of the kind.
fn width(kind: Kind) -> Width {
    match kind {
        Kind::Bool => Width::Fixed(1, 1),
        Kind::Int2 => Width::Fixed(2, 2),
        Kind::Int4 | Kind::Float4 | Kind::Date => Width::Fixed(4, 4),
        Kind::Int8 | Kind::Float8 | Kind::Time | Kind::Timestamp | Kind::TimestampTz => {
            Width::Fixed(8, 8)
        }
        Kind::Text | Kind::Bytea | Kind::Numeric(_) => Width::Varlena,
    }
}

/// The end of the data of each value of a column of the kind `Kind::Numeric(scale)`, with the values one after the other, as [`plain_lens`] takes the ends of the values of a `varlena` column.
fn numeric_ends(scale: u8, values: &[i64]) -> Vec<usize> {
    let mut end = 0;
    values
        .iter()
        .map(|&value| {
            let n = rupg_types::Numeric::from_decimal(i128::from(value), u32::from(scale));
            end += pages::numeric_len(&n);
            end
        })
        .collect()
}

impl Frozen {
    /// [`Frozen::append`] of the rows from `from` to `to` of `cells`, which are no more than the rows left in the open segment. False, with the columns as they were, when a column does not keep the values.
    fn append_cells(
        &mut self,
        cells: &Cells,
        (from, to): (usize, usize),
        tasks: Option<&dyn Tasks>,
    ) -> bool {
        let rows = self.rows;
        let kept = on_runs(tasks, &mut self.columns, |first, run| {
            run.iter_mut()
                .zip(&cells.columns[first..])
                .all(|(column, (_, values))| values.extend(Arc::make_mut(column), from, to).is_ok())
        });
        if !kept.into_iter().all(|kept| kept)
            || self.columns.iter().any(|column| column.rows != rows + to - from)
        {
            for column in &mut self.columns {
                // The new rows are open rows, so the cut does not decode.
                let _ = Arc::make_mut(column).truncate(rows);
            }
            return false;
        }
        self.rows += to - from;
        true
    }
}

impl Heap {
    /// [`Heap::insert`] of the rows of a load as cells, from the row `start` up to the first row that is longer than `TOAST_TUPLE_THRESHOLD`, for a table with no unique key. The versions that went in as values go into the encoded columns first. The values go into the encoded columns of the table on the threads of the server, and each new version has no values. The result is the number of rows, which is zero when the row `start` is long, so that the caller inserts that row as values for the toaster. `None` when the table cannot take the cells, with the heap as it was, so that the caller inserts the rows as values.
    ///
    /// # Errors
    ///
    /// `XX000` when a column does not keep its rows as it was after a failed write.
    pub(crate) fn insert_cells(
        &self,
        relation: u32,
        view: &View,
        cells: &Cells,
        start: usize,
    ) -> Result<Option<u64>> {
        if start >= cells.rows || cells.columns.iter().any(|(_, values)| values.len() != cells.rows)
        {
            return Ok(None);
        }
        let key = (view.database, relation);
        let (xmin, cmin) = (view.xid, view.command);
        let tasks = self.tasks.as_deref();
        let mut state = self.state();
        let State { tables, .. } = &mut *state;
        let table = tables.append(key);
        if !table.keys.is_empty() {
            return Ok(None);
        }
        // A `numeric` value is a `varlena` value whose length comes from its digits.
        let numeric: Vec<Option<Vec<usize>>> = cells
            .columns
            .iter()
            .map(|(kind, values)| match (kind, values) {
                (Kind::Numeric(scale), Values::Ints(values)) => Some(numeric_ends(*scale, values)),
                _ => None,
            })
            .collect();
        let columns =
            cells.columns.iter().zip(&numeric).map(|((kind, values), numeric)| match values {
                Values::Bytes(_, ends) => (width(*kind), Some(&ends[..])),
                Values::Ints(_) => (width(*kind), numeric.as_deref()),
            });
        let Some(lens) = plain_lens(columns, cells.rows, &table.fixed, &table.storage) else {
            return Ok(None);
        };
        let end = lens[start..]
            .iter()
            .position(|&len| len > TOAST_THRESHOLD)
            .map_or(cells.rows, |long| start + long);
        if end == start {
            return Ok(Some(0));
        }
        // The versions of rows that went in as values go into the columns first. When no version between the frozen ones has values, only the versions after them go in.
        let versions = table.versions.len();
        match &table.frozen {
            Some(f) if f.end == versions && f.rows == versions => {}
            Some(f) if f.rows == f.end => table.freeze_to(versions, tasks),
            None if versions == 0 => {}
            _ => table.freeze(tasks),
        }
        if table.frozen.is_none() && versions == 0 {
            let columns = cells.columns.iter();
            let columns =
                columns.map(|&(kind, _)| Arc::new(Stored::new(kind, None, Vec::new(), 0)));
            table.frozen =
                Some(Frozen { rows: 0, end: 0, columns: columns.collect(), moved: Vec::new() });
        }
        let Some(frozen) = &mut table.frozen else { return Ok(None) };
        let fits = frozen.columns.len() == cells.columns.len()
            && frozen
                .columns
                .iter()
                .zip(&cells.columns)
                .all(|(column, (kind, _))| column.kind == *kind);
        if frozen.end != versions || frozen.rows != versions || !fits {
            if versions == 0 {
                table.frozen = None;
            }
            return Ok(None);
        }
        let first = frozen.rows;
        let mut from = start;
        while from < end {
            let to = end.min(from + ROWS - frozen.rows % ROWS);
            if !frozen.append_cells(cells, (from, to), tasks) {
                for column in &mut frozen.columns {
                    Arc::make_mut(column).truncate(first).map_err(|_| {
                        Error::internal("a frozen column does not go back to its rows")
                    })?;
                }
                frozen.rows = first;
                if first == 0 {
                    table.frozen = None;
                }
                return Ok(None);
            }
            from = to;
        }
        // `RelationGetBufferForTuple` with the `BulkInsertState` of the statement, as `Table::add` places each version.
        let bulk = table.bulk.filter(|&made| made == (xmin, cmin));
        let mut ids = Vec::with_capacity(end - start);
        let mut moved = false;
        for (row, &len) in (first..).zip(&lens[start..end]) {
            let len = max_align(len);
            let (page, slot) = table.pages.place(len, None, bulk, table.save_free);
            let tid = pages::tid(table.pages.generation, page, slot);
            table.made += 1;
            let len = u16::try_from(len).unwrap_or(u16::MAX);
            let version = Version {
                tid,
                made: table.made,
                xmin,
                cmin,
                xmax: 0,
                cmax: 0,
                aborted: 0,
                next: 0,
                key_kept: false,
                len,
                values: Arc::default(),
            };
            if table.versions.last().is_none_or(|last| last.tid < tid) {
                table.versions.push(version);
            } else {
                // A version in an earlier page with free space has its row after the frozen rows, as `Table::fold` keeps it.
                let at = table.versions.partition_point(|other| other.tid < tid);
                table.versions.insert(at, version);
                if let Some(frozen) = &mut table.frozen {
                    frozen.moved.push((tid, row));
                }
                moved = true;
            }
            ids.push(tid);
        }
        if let Some(frozen) = &mut table.frozen {
            if moved {
                frozen.moved.sort_unstable_by_key(|&(tid, _)| tid);
            }
            frozen.end = table.versions.len();
        }
        state.wrote_ids(view.xid, key, &ids);
        state.loaded(view.xid, key);
        Ok(Some((end - start) as u64))
    }
}
