//! The statements that the engine plans and runs, which are `SELECT` lists without `FROM` for now, and what they read of the session.
//!
//! [`Reader`] gives the analyzer and the functions the settings, the user, the database, the time zone, the times of the transaction and the statement, and the process ID. The time zone rules of the IANA data come later, so the engine has the zones with one offset: a number of hours, an interval, a POSIX zone without daylight saving time, the names of UTC and GMT, and `Etc/GMT+N`. A function that needs another zone gives `0A000`, so it never gives a wrong time.

use std::cell::{Ref, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use rupg_analyze::{Env, Params};
use rupg_catalog::Catalog;
use rupg_common::{Error, Result, SqlState};
use rupg_exec::Plan;
use rupg_platform::Clock;
use rupg_sql::nodes::Node;
use rupg_types::{
    ByteaOutput, DateFormat, DateOrder, DateStyle, FixedZone, IntervalStyle, Recv, TimeZone, Value,
    oid,
};
use rupg_wire::Field;

use crate::guc::{self, Action, Origin, Settings, Zone};
use crate::param::type_error;
use crate::utility;

/// The schemas of a new database, from `pg_namespace.dat`.
const SCHEMAS: [(&str, u32); 3] = [("pg_catalog", 11), ("pg_toast", 99), ("public", 2200)];

/// The microseconds from 1970-01-01 to 2000-01-01, the epoch of the timestamps of PostgreSQL.
const EPOCH_SHIFT_US: i64 = 946_684_800_000_000;

/// A column of a result.
#[derive(Clone, Debug)]
pub(crate) struct Column {
    pub(crate) name: String,
    pub(crate) ty: u32,
    /// `typlen` of the type.
    pub(crate) size: i16,
    pub(crate) typmod: i32,
    /// The OID of the table and the attribute number when the column is a column of a table, or zeros.
    pub(crate) origin: (u32, i16),
}

impl Column {
    /// A column of type `text`, as the utility statements give.
    pub(crate) fn text(name: String) -> Column {
        Column { name, ty: oid::TEXT, size: -1, typmod: -1, origin: (0, 0) }
    }

    /// The field of `RowDescription` in a format.
    pub(crate) fn field(&self, format: i16) -> Field<'_> {
        Field {
            name: self.name.as_bytes(),
            table: self.origin.0,
            column: self.origin.1,
            type_oid: self.ty,
            type_size: self.size,
            type_modifier: self.typmod,
            format,
        }
    }
}

/// A row of a result: the bytes of each value in the format of its column, or `None` for a null value.
pub(crate) type Row = Vec<Option<Vec<u8>>>;

/// True for a statement that the engine plans, not the session.
pub(crate) fn is_query(node: &Node) -> bool {
    matches!(node, Node::SelectStmt(_))
}

/// The time on the clock in microseconds since 2000-01-01 UTC.
pub(crate) fn now(clock: &dyn Clock) -> i64 {
    i64::try_from(clock.wall_us()).unwrap_or(i64::MAX).saturating_sub(EPOCH_SHIFT_US)
}

/// What a statement reads of the session, and the settings that `set_config` changes.
#[derive(Debug)]
pub(crate) struct Reader<'a> {
    settings: &'a RefCell<Settings>,
    pub(crate) user: &'a str,
    pub(crate) database: &'a str,
    pub(crate) transaction_start: i64,
    pub(crate) statement_start: i64,
    pub(crate) clock: &'a dyn Clock,
    pub(crate) pid: i32,
    /// The catalog that the statement sees: the catalog of the last commit, or the copy of the transaction that changes it.
    pub(crate) catalog: Arc<Catalog>,
    /// The text of `TimeZone` and its zone, or the name of a zone whose rules the engine does not have yet. A call of `set_config` can change the setting, so the zone is made again when the text changes.
    zone: RefCell<(String, ZoneOf)>,
}

/// The zone of a `TimeZone` value, or the name of a zone whose rules the engine does not have yet.
type ZoneOf = std::result::Result<Rc<FixedZone>, String>;

impl<'a> Reader<'a> {
    pub(crate) fn new(
        settings: &'a RefCell<Settings>,
        user: &'a str,
        database: &'a str,
        times: (i64, i64),
        clock: &'a dyn Clock,
        pid: i32,
        catalog: Arc<Catalog>,
    ) -> Reader<'a> {
        let name = settings.borrow().get("TimeZone").unwrap_or_else(|| "UTC".to_owned());
        let zone = fixed_zone(&name).map(Rc::new);
        Reader {
            settings,
            user,
            database,
            transaction_start: times.0,
            statement_start: times.1,
            clock,
            pid,
            catalog,
            zone: RefCell::new((name, zone)),
        }
    }

    /// The settings of the session.
    pub(crate) fn settings(&self) -> Ref<'_, Settings> {
        self.settings.borrow()
    }

    /// The schemas of `search_path` that exist, in order, without the implicit schemas, as `recomputeNamespacePath` finds them. `$user` names the schema of the user, and `pg_temp` the temporary schema, which do not exist yet.
    fn path(&self) -> Vec<(String, u32)> {
        let text = self.settings().get("search_path").unwrap_or_default();
        let mut path: Vec<(String, u32)> = Vec::new();
        for name in guc::split_identifiers(&text, ',').unwrap_or_default() {
            let name = if name == "$user" { self.user.to_owned() } else { name };
            if let Some(oid) = self.schema(&name)
                && !path.iter().any(|&(_, o)| o == oid)
            {
                path.push((name, oid));
            }
        }
        path
    }

    /// The OID of a built-in schema or a schema of the catalog.
    fn schema(&self, name: &str) -> Option<u32> {
        SCHEMAS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, oid)| *oid)
            .or_else(|| self.catalog.schema_by_name(name).map(|s| s.oid))
    }
}

/// The zone of a `TimeZone` value that the setting accepted, or the name of a zone that the engine does not have yet.
fn fixed_zone(text: &str) -> std::result::Result<FixedZone, String> {
    match guc::zone(text) {
        Ok((Zone::Fixed { offset, abbrev }, _)) => Ok(FixedZone { offset, abbrev }),
        Ok((Zone::Named(name), _)) => named_zone(name).ok_or_else(|| name.to_owned()),
        Err(_) => Err(text.to_owned()),
    }
}

/// The zones of the IANA data with one offset: the names of UTC and GMT, and `Etc/GMT+N`, whose offset is west of UTC, with the abbreviation `%z` of the data.
fn named_zone(name: &str) -> Option<FixedZone> {
    let upper = name.to_ascii_uppercase();
    let base = upper.strip_prefix("ETC/").unwrap_or(&upper);
    match base {
        "UTC" | "UCT" | "UNIVERSAL" | "ZULU" => return Some(FixedZone::utc()),
        "GMT" | "GMT0" | "GMT+0" | "GMT-0" | "GREENWICH" => {
            return Some(FixedZone { offset: 0, abbrev: "GMT".to_owned() });
        }
        _ => {}
    }
    let hours: i32 = upper.strip_prefix("ETC/GMT")?.parse().ok()?;
    let sign = if hours > 0 { '-' } else { '+' };
    Some(FixedZone { offset: -hours * 3600, abbrev: format!("{sign}{:02}", hours.abs()) })
}

/// `DateStyle` in its canonical form, such as `ISO, MDY`.
fn date_format(text: &str) -> DateFormat {
    let (style, order) = text.split_once(", ").unwrap_or((text, "MDY"));
    let style = match style {
        "SQL" => DateStyle::Sql,
        "Postgres" => DateStyle::Postgres,
        "German" => DateStyle::German,
        _ => DateStyle::Iso,
    };
    let order = match order {
        "DMY" => DateOrder::Dmy,
        "YMD" => DateOrder::Ymd,
        _ => DateOrder::Mdy,
    };
    DateFormat { style, order }
}

impl rupg_func::Session for Reader<'_> {
    fn date_format(&self) -> DateFormat {
        self.settings().get("DateStyle").map_or(DateFormat::ISO_MDY, |text| date_format(&text))
    }

    fn interval_style(&self) -> IntervalStyle {
        match self.settings().get("IntervalStyle").as_deref() {
            Some("postgres_verbose") => IntervalStyle::PostgresVerbose,
            Some("sql_standard") => IntervalStyle::SqlStandard,
            Some("iso_8601") => IntervalStyle::Iso8601,
            _ => IntervalStyle::Postgres,
        }
    }

    fn zone(&self) -> Result<Rc<dyn TimeZone>> {
        let name = self.settings().get("TimeZone").unwrap_or_else(|| "UTC".to_owned());
        let mut zone = self.zone.borrow_mut();
        if zone.0 != name {
            let made = fixed_zone(&name).map(Rc::new);
            *zone = (name, made);
        }
        match &zone.1 {
            Ok(zone) => Ok(Rc::clone(zone) as Rc<dyn TimeZone>),
            Err(name) => Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("the time zone \"{name}\" is not supported yet"),
            )
            .with_hint("Set TimeZone to UTC or to a fixed offset, such as SET TIME ZONE '+02'.")),
        }
    }

    fn extra_float_digits(&self) -> i32 {
        self.settings().get("extra_float_digits").and_then(|v| v.parse().ok()).unwrap_or(1)
    }

    fn bytea_output(&self) -> ByteaOutput {
        match self.settings().get("bytea_output").as_deref() {
            Some("escape") => ByteaOutput::Escape,
            _ => ByteaOutput::Hex,
        }
    }

    fn array_nulls(&self) -> bool {
        self.settings().get("array_nulls").as_deref() != Some("off")
    }

    fn transaction_start(&self) -> i64 {
        self.transaction_start
    }

    fn statement_start(&self) -> i64 {
        self.statement_start
    }

    fn clock(&self) -> i64 {
        now(self.clock)
    }

    fn user(&self) -> &str {
        self.user
    }

    fn session_user(&self) -> &str {
        self.user
    }

    fn database(&self) -> &str {
        self.database
    }

    fn catalog(&self) -> Option<&Catalog> {
        Some(&self.catalog)
    }

    fn schemas(&self) -> Vec<String> {
        self.path().into_iter().map(|(name, _)| name).collect()
    }

    fn backend_pid(&self) -> i32 {
        self.pid
    }

    fn version(&self) -> String {
        crate::connection::version_text()
    }

    fn setting(&self, name: &str) -> Option<String> {
        self.settings().get(name)
    }

    fn set_setting(&self, name: &str, value: Option<&str>, local: bool) -> Result<String> {
        if let Some(value) = value {
            utility::check_role(name, value, self.user)?;
        }
        let action = if local { Action::Local } else { Action::Set };
        let mut settings = self.settings.borrow_mut();
        settings.set(name, value, action, Origin::Statement)?;
        Ok(settings.show(name)?.1)
    }
}

impl Env for Reader<'_> {
    fn search_path(&self) -> Vec<u32> {
        self.path().into_iter().map(|(_, oid)| oid).collect()
    }

    fn namespace(&self, name: &str) -> Option<u32> {
        self.schema(name)
    }

    fn database(&self) -> String {
        self.database.to_owned()
    }

    fn input(&self, ty: u32, text: &str, typmod: i32) -> Result<Value> {
        rupg_func::input(ty, text, typmod, self)
    }

    fn catalog(&self) -> Option<&Catalog> {
        Some(&self.catalog)
    }

    fn allow_system_table_mods(&self) -> bool {
        self.settings().get("allow_system_table_mods").as_deref() == Some("on")
    }
}

/// `parse_analyze` and the plan of a statement.
///
/// # Errors
///
/// The errors of the analysis, and `0A000` for a part of the statement that the engine cannot run yet.
pub(crate) fn plan(node: &Node, reader: &Reader<'_>, params: &Params) -> Result<Plan> {
    rupg_exec::prepare(rupg_analyze::analyze(node, reader, params)?)
}

/// The columns of the result of a plan.
pub(crate) fn columns(plan: &Plan) -> Vec<Column> {
    plan.columns()
        .iter()
        .map(|target| Column {
            name: target.name.clone(),
            ty: target.expr.ty,
            size: rupg_pgcatalog::builtin::type_by_oid(target.expr.ty).map_or(-1, |row| row.len),
            typmod: target.expr.typmod,
            origin: target.origin.unwrap_or_default(),
        })
        .collect()
}

/// Runs a plan and gives its rows in the result format of each column: 1 is binary and any other code is text. `Execute` checks the codes when it sends the first row.
///
/// # Errors
///
/// The errors of the functions of the plan and of the output functions.
pub(crate) fn run(
    plan: &Plan,
    params: &[Value],
    reader: &Reader<'_>,
    formats: &[i16],
) -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    for values in plan.run(params, reader)? {
        let mut row = Vec::with_capacity(values.len());
        for (i, (target, value)) in plan.columns().iter().zip(&values).enumerate() {
            if matches!(value, Value::Null) {
                row.push(None);
                continue;
            }
            let mut bytes = Vec::new();
            if formats.get(i) == Some(&1) {
                rupg_func::send(target.expr.ty, value, &mut bytes)?;
            } else {
                rupg_func::output(target.expr.ty, value, reader, &mut bytes)?;
            }
            row.push(Some(bytes));
        }
        rows.push(row);
    }
    Ok(rows)
}

/// The value of parameter `number`, from 1, of a `Bind`: the input function of the type for the text format and the receive function for the binary format.
///
/// # Errors
///
/// `22023` for a format code that is not 0 or 1, and the error of the type function for a bad value.
pub(crate) fn param(
    ty: u32,
    format: i16,
    value: Option<&[u8]>,
    number: usize,
    reader: &Reader<'_>,
) -> Result<Value> {
    match format {
        0 | 1 if value.is_none() => Ok(Value::Null),
        0 => {
            let text = Recv::new(value.unwrap_or_default()).text().map_err(type_error)?;
            rupg_func::input(ty, text, -1, reader)
        }
        1 => rupg_func::receive(ty, value.unwrap_or_default(), -1, number),
        _ => Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("unsupported format code: {format}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zones_with_one_offset() {
        assert_eq!(fixed_zone("UTC").unwrap().abbrev, "UTC");
        assert_eq!(fixed_zone("Etc/Zulu").unwrap().offset, 0);
        assert_eq!(fixed_zone("Greenwich").unwrap().abbrev, "GMT");
        let zone = fixed_zone("Etc/GMT+5").unwrap();
        assert_eq!((zone.offset, zone.abbrev.as_str()), (-18000, "-05"));
        let zone = fixed_zone("etc/gmt-14").unwrap();
        assert_eq!((zone.offset, zone.abbrev.as_str()), (50400, "+14"));
        assert_eq!(fixed_zone("+02").unwrap().offset, 7200);
        assert_eq!(fixed_zone("Europe/Paris").unwrap_err(), "Europe/Paris");
    }

    #[test]
    fn date_styles() {
        assert_eq!(date_format("ISO, MDY"), DateFormat::ISO_MDY);
        let german = date_format("German, DMY");
        assert_eq!((german.style, german.order), (DateStyle::German, DateOrder::Dmy));
    }
}
