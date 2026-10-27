//! The function call protocol: `FunctionCall`, as `HandleFunctionRequest` of `fastpath.c` runs it. The session runs the function as the target of a `SELECT` with a parameter of the type of each argument, so the checks of the privileges and the run of the function are the ones of a query. psql uses the protocol for `\lo_import`, `\lo_export` and `\lo_unlink`.

use rupg_analyze::Params;
use rupg_common::{Error, SqlState};
use rupg_pgcatalog::builtin;
use rupg_types::{Value, oid};
use rupg_wire::{FunctionCall, OutBuf};

use super::Connection;
use super::extended::{Failed, aborted};
use crate::query;

impl Connection {
    /// `HandleFunctionRequest`: `fetch_fp_info` of the function, `parse_fcall_arguments`, the call and `SendFunctionResult`.
    pub(super) fn function_call(
        &mut self,
        call: FunctionCall<'_>,
        out: &mut OutBuf,
    ) -> Result<(), Failed> {
        self.start_xact();
        if self.transaction.failed() {
            return Err(aborted().into());
        }
        self.settings.borrow_mut().take_snapshot();
        let fid = call.oid;
        let argtypes = match self.catalog().function(fid) {
            Some(function) => function.argtypes.clone(),
            None => match builtin::proc_by_oid(fid) {
                Some(row) => row.argtypes.to_vec(),
                None => {
                    return Err(Error::new(
                        SqlState::UNDEFINED_FUNCTION,
                        format!("function with OID {fid} does not exist"),
                    )
                    .into());
                }
            },
        };
        let reader = self.reader();
        let mut name = Vec::new();
        rupg_func::output(oid::REGPROC, &Value::Oid(fid), &reader, &mut name)?;
        let mut args = call.args(argtypes.len())?;
        let formats = args.formats;
        let mut params = Vec::with_capacity(argtypes.len());
        for (i, &ty) in argtypes.iter().enumerate() {
            let value = args.next().transpose()?.flatten();
            params.push(query::param(ty, formats.of(i), value, i + 1, &reader)?);
        }
        let format = args.finish()?;
        let list: Vec<String> = (1..=argtypes.len()).map(|i| format!("${i}")).collect();
        let text = format!("SELECT {}({})", String::from_utf8_lossy(&name), list.join(", "));
        let (parsed, _) = rupg_sql::parse(&text).map_err(Error::from)?;
        let Some(&(node, _)) = super::statements(&parsed, &text).first() else {
            return Err(Error::internal("a statement").into());
        };
        let plan = query::plan(node, &reader, &Params { types: argtypes, variable: false })?;
        let plan = plan.fold(Some(&params), &reader)?;
        plan.start(&reader)?;
        if !matches!(format, 0 | 1) {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("unsupported format code: {format}"),
            )
            .into());
        }
        let rows = query::run(&plan, &params, &reader, &[format])?;
        drop(reader);
        let result = rows.into_iter().next().and_then(|row| row.into_iter().next()).flatten();
        out.function_call_response(result.as_deref());
        Ok(())
    }
}
