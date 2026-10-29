//! The values of the parameters in one session, with the rules of `SET`, `RESET` and `SHOW`.
//!
//! A parameter that the session did not change has its boot value and has no slot, so a new session costs nothing for the four hundred parameters that it does not touch. A change in a transaction keeps the value from before the transaction, and the end of the transaction keeps or restores it with the rules of `push_old_value` and `AtEOXact_GUC` in `guc.c`: a `SET` that commits stays, a `SET LOCAL` goes back at the end, a `SET LOCAL` after a `SET` goes back to the value of the `SET`, and an abort goes back to the value from before the transaction.
//!
//! The session also tracks which reported values changed, so that the server sends one `ParameterStatus` message for each before `ReadyForQuery`, in the order of PostgreSQL.
//!
//! Lifted from `crates/rudb-common/src/guc/settings.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use super::{Context, Parameter, Setting, check, find, flag, valid_custom_name};
use rupg_common::Error;
use rupg_common::SqlState;
use rupg_platform::TcpOption;

/// Where a value came from, which `RESET ALL` reads to find the values that a statement set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The boot value or a value that the server set.
    Default,
    /// A value of `postgresql.conf`, which a reload of the file can change.
    File,
    /// A value of the command line of the server.
    Argument,
    /// A value that `ALTER ROLE ALL SET` stored for all the databases and all the roles.
    Global,
    /// A value that `ALTER DATABASE ... SET` stored for the database of the session.
    Database,
    /// A value that `ALTER ROLE ... SET` stored for the user of the session.
    User,
    /// A value that `ALTER ROLE ... IN DATABASE ... SET` stored for the user and the database of the session.
    DatabaseUser,
    /// A value of the startup packet.
    Client,
    /// A value that the server sets over the others at start, such as `config_file`.
    Override,
    /// A value that a statement set.
    Session,
}

impl Source {
    /// The text of the `source` column of `pg_settings`, from `GucSource_Names` in `guc_tables.c`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::File => "configuration file",
            Source::Argument => "command line",
            Source::Global => "global",
            Source::Database => "database",
            Source::User => "user",
            Source::DatabaseUser => "database user",
            Source::Client => "client",
            Source::Override => "override",
            Source::Session => "session",
        }
    }
}

/// The columns of a row of `pg_settings` that change with the values of the session.
#[derive(Clone, Debug)]
pub struct Current {
    /// The parameter of the row.
    pub parameter: &'static Parameter,
    /// The `setting` column: the value in the base unit, or the text of the show hook of the parameter.
    pub setting: String,
    /// The `source` column.
    pub source: &'static str,
    /// The `reset_val` column, or `None` for a string parameter that the server sets at start and that has no value yet.
    pub reset: Option<String>,
    /// The `sourcefile` and `sourceline` columns: the line of the configuration file that gave the value. `None` when the value did not come from the file, or when the session cannot read the settings of the server.
    pub location: Option<(String, i32)>,
}

/// What a statement does with a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// `SET` and `RESET`: the value stays after the transaction commits.
    Set,
    /// `SET LOCAL`: the value stays until the end of the transaction.
    Local,
}

/// Who sets a value, for the rules of the context of a parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// The startup packet, before the session starts.
    Startup,
    /// A statement of the session.
    Statement,
}

/// One argument of `SET`, as the grammar of PostgreSQL gives it to `flatten_set_variable_args`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Arg {
    /// An integer, as written, with its sign.
    Integer(String),
    /// A number with a fraction or an exponent, as written, with its sign.
    Number(String),
    /// A string, a name, or a key word such as `on`.
    String(String),
    /// `INTERVAL '...'`, with the text of the interval.
    Interval(String),
}

/// `flatten_set_variable_args`: the arguments of `SET name TO ...` as one text value.
///
/// # Errors
///
/// A list of more than one value for a parameter that does not take a list.
pub fn flatten(name: &str, args: &[Arg]) -> Result<String, Error> {
    let flags = find(name).map_or(0, |parameter| parameter.flags);
    if args.len() > 1 && flags & flag::LIST_INPUT == 0 {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("SET {name} takes only one argument"),
        ));
    }
    let mut text = String::new();
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            text.push_str(", ");
        }
        match arg {
            Arg::Integer(value) => match value.parse::<i64>() {
                Ok(value) => text.push_str(&value.to_string()),
                Err(_) => text.push_str(value),
            },
            Arg::Number(value) => text.push_str(value),
            Arg::String(value) if flags & flag::LIST_QUOTE != 0 => {
                text.push_str(&quote_identifier(value));
            }
            Arg::String(value) => text.push_str(value),
            Arg::Interval(value) => {
                text.push_str("INTERVAL '");
                text.push_str(value);
                text.push('\'');
            }
        }
    }
    Ok(text)
}

/// `quote_identifier`: the name in double quotes if it is not a simple name in lower case.
///
/// PostgreSQL also quotes a reserved key word. The list values that this quotes are schema names and library names, where a reserved word in a list is rare, so this does not check for one.
fn quote_identifier(name: &str) -> String {
    let simple = name.bytes().next().is_some_and(|c| c.is_ascii_lowercase() || c == b'_')
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'$');
    if simple { name.to_owned() } else { format!("\"{}\"", name.replace('"', "\"\"")) }
}

/// A value and where it came from.
#[derive(Clone, Debug, PartialEq)]
struct Value {
    setting: Setting,
    source: Source,
}

/// What the current transaction did to a parameter, from `GucStackState` in `guc.h`.
#[derive(Clone, Debug)]
enum State {
    /// A `SET`.
    Set,
    /// A `SET LOCAL`.
    Local,
    /// A `SET LOCAL` after a `SET`, with the value of the `SET`.
    SetLocal(Value),
}

/// The value from before the transaction and what the transaction did.
#[derive(Clone, Debug)]
struct Save {
    prior: Value,
    state: State,
}

/// A parameter that the session changed, or a placeholder for a custom parameter.
#[derive(Clone, Debug)]
struct Slot {
    /// `None` for a placeholder.
    parameter: Option<&'static Parameter>,
    /// The name of a placeholder as it was first set.
    name: String,
    current: Value,
    reset: Value,
    save: Option<Save>,
    /// The file and the line of the configuration file that last gave a value, as `set_config_sourcefile` keeps them.
    location: Option<(Arc<str>, i32)>,
    /// The number of slots before this one. `RESET ALL` goes in this order, as PostgreSQL goes over the parameters in the order that they left their default values.
    order: usize,
}

/// The values from before the settings of a function, which `NewGUCNestLevel` and `GUC_ACTION_SAVE` keep for `AtEOXact_GUC` at the end of the call.
#[derive(Clone, Debug, Default)]
struct Frame {
    /// The slot of each name of the settings before the call, or `None` for a name with no slot.
    saved: Vec<(String, Option<Slot>)>,
    /// The names that a `SET` changed during the call, which keep their values after it.
    kept: Vec<String>,
    /// The OID and the name of the owner of a `SECURITY DEFINER` function, which is the current user during the call.
    definer: Option<(u32, String)>,
}

/// The order in which PostgreSQL 19 sends the reported parameters at startup, which is the order of its hash table.
const STARTUP_REPORTS: [&str; 15] = [
    "IntervalStyle",
    "search_path",
    "is_superuser",
    "standard_conforming_strings",
    "session_authorization",
    "client_encoding",
    "server_version",
    "server_encoding",
    "in_hot_standby",
    "integer_datetimes",
    "TimeZone",
    "application_name",
    "default_transaction_read_only",
    "scram_iterations",
    "DateStyle",
];

/// `name` in lower case, which is the key of its slot. Most names are in lower case already, and for those this makes no new string.
fn lower(name: &str) -> Cow<'_, str> {
    if name.bytes().any(|c| c.is_ascii_uppercase()) {
        Cow::Owned(name.to_ascii_lowercase())
    } else {
        Cow::Borrowed(name)
    }
}

/// The slot of a name in lower case to change, copied from `base` when it is only there.
fn owned<'a>(
    slots: &'a mut Arc<HashMap<String, Slot>>,
    base: &HashMap<String, Slot>,
    key: &str,
) -> Option<&'a mut Slot> {
    let slots = Arc::make_mut(slots);
    if !slots.contains_key(key) {
        slots.insert(key.to_owned(), base.get(key)?.clone());
    }
    slots.get_mut(key)
}

/// The parameters of a transaction and the parameters of their defaults. The options of each pair are the same, so a value of one is a value of the other.
const CHARACTERISTICS: [(&str, &str); 3] = [
    ("transaction_isolation", "default_transaction_isolation"),
    ("transaction_read_only", "default_transaction_read_only"),
    ("transaction_deferrable", "default_transaction_deferrable"),
];

/// The values of the parameters of a transaction, from [`Settings::characteristics`].
#[derive(Clone, Debug, PartialEq)]
pub struct Characteristics([Setting; 3]);

/// The values of the parameters at a savepoint, from [`Settings::savepoint`].
#[derive(Clone, Debug)]
pub struct Savepoint {
    slots: Arc<HashMap<String, Slot>>,
    saved: Vec<String>,
    read_only: Setting,
}

/// The prefix that rupg keeps for its own parameters.
const RESERVED_PREFIX: &str = "rupg";

/// The parameter values of one session.
#[derive(Clone, Debug)]
pub struct Settings {
    /// The slots that [`Settings::share`] moved here, which the server makes once for every session. A session does not change them. It copies a slot to `slots` before it changes it.
    base: Arc<HashMap<String, Slot>>,
    /// The changed parameters and the placeholders, by name in lower case, over the ones in `base`. Shared between the copies of one session's settings until one of them changes a value, because the server copies the settings for the engine at each change.
    slots: Arc<HashMap<String, Slot>>,
    /// The number of names in `base` and `slots` together, which gives the order of a new slot.
    count: usize,
    /// The names of the slots that the current transaction changed.
    saved: Vec<String>,
    /// The values from before the settings of each function that runs, the innermost one last.
    frames: Vec<Frame>,
    /// The names of the reported parameters that changed since the last report, the first one first.
    pending: Vec<&'static str>,
    /// The value of each reported parameter that the client last got.
    reported: Arc<HashMap<&'static str, String>>,
    generation: u64,
    superuser: bool,
    /// `FirstSnapshotSet`: a statement of the current transaction took a snapshot, so the isolation level and the deferrable mode cannot change.
    snapshot: bool,
    /// `IsSubTransaction`: the transaction has a savepoint, so the isolation level and the deferrable mode cannot change.
    subtransaction: bool,
    /// The values that the system gave to the TCP options of the socket of the session, in the order of [`TcpOption::ALL`], with -1 for a value that the system did not give. `None` for a Unix socket and for a session with no socket.
    socket: Option<[i32; 4]>,
    /// The value of a `SET seed` that `random` did not use yet. `assign_random_seed` calls `setseed` once for each value that a statement sets, and a rollback does not undo the seed.
    seed: Option<f64>,
    /// A statement, a rollback or the end of a transaction gave a value to `role` or `session_authorization`, so their assign hooks run and set `is_superuser` again.
    roles_set: bool,
}

/// Whether the key is the key of `role` or `session_authorization`.
fn is_role_key(key: &str) -> bool {
    matches!(key, "role" | "session_authorization")
}
impl Settings {
    /// The boot values, for a session of a superuser or of another user.
    #[must_use]
    pub fn new(superuser: bool) -> Self {
        Self {
            base: Arc::default(),
            slots: Arc::default(),
            count: 0,
            saved: Vec::new(),
            frames: Vec::new(),
            pending: Vec::new(),
            reported: Arc::default(),
            generation: 0,
            superuser,
            snapshot: false,
            subtransaction: false,
            socket: None,
            seed: None,
            roles_set: false,
        }
    }

    /// Moves the slots of these settings to the part that the copies share and do not change. The server does this to the values that each session starts with, so that a session holds only the slots that it changes. Call it outside of a transaction.
    pub fn share(&mut self) {
        if self.slots.is_empty() {
            return;
        }
        let own = std::mem::take(&mut self.slots);
        let own = Arc::try_unwrap(own).unwrap_or_else(|own| (*own).clone());
        Arc::make_mut(&mut self.base).extend(own);
    }

    /// The slot of a name in lower case, from `slots` or else from `base`.
    fn find_slot(&self, key: &str) -> Option<&Slot> {
        self.slots.get(key).or_else(|| self.base.get(key))
    }

    /// Each slot with its name, once for each name.
    fn each_slot(&self) -> impl Iterator<Item = (&String, &Slot)> {
        let base = self.base.iter().filter(|(key, _)| !self.slots.contains_key(key.as_str()));
        self.slots.iter().chain(base)
    }

    /// Keeps the values that the system gave to the TCP options of the socket of the session, in the order of [`TcpOption::ALL`], for the show hooks of the TCP parameters.
    pub fn set_socket(&mut self, values: [i32; 4]) {
        self.socket = Some(values);
    }

    /// The values of the TCP parameters, in the order of [`TcpOption::ALL`]. 0 asks for the value of the system.
    #[must_use]
    pub fn tcp_values(&self) -> [i32; 4] {
        TcpOption::ALL.map(|option| match find(option.parameter()).map(|p| self.setting(p)) {
            Some(Setting::Int(value)) => value,
            _ => 0,
        })
    }

    /// The OID and the name of the current user of the innermost call of a `SECURITY DEFINER` function, or `None` outside such a call.
    pub fn definer(&self) -> Option<&(u32, String)> {
        self.frames.iter().rev().find_map(|frame| frame.definer.as_ref())
    }

    /// The check of `GUC_NOT_WHILE_SEC_REST` in `set_config_option`: `InLocalUserIdChange` is true in a call of a `SECURITY DEFINER` function, and then `role` and `session_authorization` cannot change, because the end of the call does not tell the settings.
    ///
    /// # Errors
    ///
    /// `42501` for `role` or `session_authorization` in such a call.
    pub fn check_definer(&self, name: &str) -> Result<(), Error> {
        let Some(parameter) = find(name) else { return Ok(()) };
        if matches!(parameter.name, "role" | "session_authorization") && self.definer().is_some() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!(
                    "cannot set parameter \"{}\" within security-definer function",
                    parameter.name
                ),
            ));
        }
        Ok(())
    }

    /// Changes whether the session counts as a superuser for the parameters that only a superuser can set, after `SET ROLE` or `SET SESSION AUTHORIZATION` changes the current user.
    pub fn set_superuser(&mut self, superuser: bool) {
        self.superuser = superuser;
    }

    /// A number that changes each time a value changes, so that a reader can keep what it made from the values until the next change.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Sets a value that the server owns, such as `server_version`, as the current value and the reset value. There is no check of the context.
    ///
    /// # Errors
    ///
    /// A name that is not a parameter, or a value that the parameter does not take.
    pub fn set_internal(&mut self, name: &str, text: &str) -> Result<(), Error> {
        let parameter = find(name).ok_or_else(|| unrecognized(name))?;
        let setting = self.parse(parameter, text)?;
        let value = Value { setting, source: Source::Default };
        self.store_reset(parameter, value);
        Ok(())
    }

    /// Sets a value that the server works out at start, such as `config_file`, as the current value and the reset value, with the source `override`. The values of the file and of the command line do not change it after this. There is no check of the context.
    ///
    /// # Errors
    ///
    /// A name that is not a parameter, or a value that the parameter does not take.
    pub fn set_override(&mut self, name: &str, text: &str) -> Result<(), Error> {
        let parameter = find(name).ok_or_else(|| unrecognized(name))?;
        let setting = self.parse(parameter, text)?;
        self.store_reset(parameter, Value { setting, source: Source::Override });
        Ok(())
    }

    /// `set_config_sourcefile`: the file and the line of the configuration file that gave the value of a parameter or a placeholder, which `pg_settings` shows while the value comes from the file.
    pub fn set_location(&mut self, name: &str, file: &str, line: i32) {
        let key =
            find(name).map_or_else(|| name.to_ascii_lowercase(), |p| p.name.to_ascii_lowercase());
        if let Some(slot) = owned(&mut self.slots, &self.base, &key) {
            slot.location = Some((Arc::from(file), line));
        }
    }

    /// True for a parameter or a placeholder that exists, as `find_option` finds it without making a placeholder.
    #[must_use]
    pub fn exists(&self, name: &str) -> bool {
        find(name).is_some() || self.find_slot(lower(name).as_ref()).is_some()
    }

    /// `set_config_option` with `PGC_SIGHUP` that changes nothing, as `pg_file_settings` checks a line of the configuration file. The value must be valid, an internal parameter cannot come from the file, and a parameter of the server start must keep its current value.
    ///
    /// # Errors
    ///
    /// A name that is not a parameter and not a valid placeholder name, a value that the parameter does not take, and a value that cannot change now.
    pub fn check_file(&self, name: &str, text: &str) -> Result<(), Error> {
        let Some(parameter) = find(name) else {
            return placeholder_name(name);
        };
        let cannot = |what: &str| {
            Error::new(
                SqlState::CANT_CHANGE_RUNTIME_PARAM,
                format!("parameter \"{}\" {what}", parameter.name),
            )
        };
        if parameter.context == Context::Internal {
            return Err(cannot("cannot be changed"));
        }
        let setting = self.parse(parameter, text)?;
        if parameter.context == Context::Postmaster
            && parameter.show(&setting) != parameter.show(&self.setting(parameter))
        {
            return Err(cannot("cannot be changed without restarting the server"));
        }
        Ok(())
    }

    /// A value of the configuration file, as `set_config_option` sets it with `PGC_S_FILE`. The value becomes the reset value and the current value, unless the command line, the client or a statement set them. `None` is the boot value, for a parameter that left the file. A name with a dot that is not a parameter is a placeholder. There is no check of the context. The result is true when the current value took the value, which is when PostgreSQL logs the change.
    ///
    /// # Errors
    ///
    /// A name that is not a parameter and not a valid placeholder name, or a value that the parameter does not take.
    pub fn set_file(&mut self, name: &str, text: Option<&str>) -> Result<bool, Error> {
        let (key, parameter, setting) = match find(name) {
            Some(parameter) => {
                let setting = match text {
                    Some(text) => self.parse(parameter, text)?,
                    None => parameter.boot(),
                };
                (parameter.name.to_ascii_lowercase(), Some(parameter), setting)
            }
            None => {
                placeholder_name(name)?;
                (name.to_ascii_lowercase(), None, Setting::String(text.unwrap_or("").to_owned()))
            }
        };
        let source = if text.is_some() { Source::File } else { Source::Default };
        let value = Value { setting, source };
        let slot = self.slot(&key, parameter, parameter.map_or(name, |parameter| parameter.name));
        let from_file = |value: &Value| matches!(value.source, Source::Default | Source::File);
        if from_file(&slot.reset) {
            slot.reset = value.clone();
        }
        if let Some(save) = &mut slot.save {
            // In a transaction block, the value goes under the value of the transaction.
            if from_file(&save.prior) {
                save.prior = value;
            }
            return Ok(false);
        }
        if !from_file(&slot.current) {
            return Ok(false);
        }
        if slot.current != value {
            slot.current = value;
            self.changed(parameter);
        }
        Ok(true)
    }

    /// A value of the command line of the server, as the current value and the reset value. It comes before the values of the client and after the values of the file. There is no check of the context.
    ///
    /// # Errors
    ///
    /// A name that is not a parameter and not a valid placeholder name, or a value that the parameter does not take.
    pub fn set_argument(&mut self, name: &str, text: &str) -> Result<(), Error> {
        let Some(parameter) = find(name) else {
            placeholder_name(name)?;
            let key = name.to_ascii_lowercase();
            let value =
                Value { setting: Setting::String(text.to_owned()), source: Source::Argument };
            let slot = self.slot(&key, None, name);
            slot.current = value.clone();
            slot.reset = value;
            return Ok(());
        };
        let setting = self.parse(parameter, text)?;
        self.store_reset(parameter, Value { setting, source: Source::Argument });
        Ok(())
    }

    /// A value that `ALTER DATABASE ... SET` or `ALTER ROLE ... SET` stored for the session, as `ApplySetting` sets it with the source of the row of `pg_db_role_setting` at the start of the session. It comes before the values of the client. A parameter that a statement cannot set is not set.
    ///
    /// # Errors
    ///
    /// A name that is not a parameter and not a valid placeholder name, a parameter that cannot change after the start of the session, or a value that the parameter does not take.
    pub fn set_database(&mut self, name: &str, text: &str, source: Source) -> Result<(), Error> {
        let Some(parameter) = find(name) else {
            placeholder_name(name)?;
            let key = name.to_ascii_lowercase();
            let value = Value { setting: Setting::String(text.to_owned()), source };
            let slot = self.slot(&key, None, name);
            slot.current = value.clone();
            slot.reset = value;
            return Ok(());
        };
        self.allowed(parameter, Origin::Statement)?;
        let setting = self.parse(parameter, text)?;
        self.store_reset(parameter, Value { setting, source });
        Ok(())
    }

    /// `validate_option_array_item` for `ALTER DATABASE ... SET`: the check of the name, of the context and of the value, with no change. `text` is `None` for `RESET`. It gives the name that the stored setting has, which is the name of the parameter as PostgreSQL spells it.
    ///
    /// # Errors
    ///
    /// A name that is not a parameter and not a valid placeholder name, a parameter that a statement cannot set, and a value that the parameter does not take.
    pub fn check_stored(&self, name: &str, text: Option<&str>) -> Result<String, Error> {
        let Some(parameter) = find(name) else {
            if text.is_some() || !valid_custom_name(name) {
                placeholder_name(name)?;
            }
            return Ok(name.to_owned());
        };
        self.allowed(parameter, Origin::Statement)?;
        if let Some(text) = text {
            self.parse(parameter, text)?;
        }
        Ok(parameter.name.to_owned())
    }

    /// Reads a value of a parameter with the rules of `set`, and does not keep it.
    ///
    /// # Errors
    ///
    /// A value that the parameter does not take.
    pub fn check(&self, parameter: &'static Parameter, text: &str) -> Result<Setting, Error> {
        self.parse(parameter, text)
    }

    /// The names of the parameters and placeholders whose reset value came from the configuration file, in the order in which they first changed.
    #[must_use]
    pub fn file_names(&self) -> Vec<String> {
        let mut names: Vec<(usize, String)> = self
            .each_slot()
            .map(|(_, slot)| slot)
            .filter(|slot| slot.reset.source == Source::File)
            .map(|slot| (slot.order, slot.name.clone()))
            .collect();
        names.sort();
        names.into_iter().map(|(_, name)| name).collect()
    }

    /// `SET name TO text`, `SET name TO DEFAULT` and `RESET name`, where `text` is `None` for the last two.
    ///
    /// # Errors
    ///
    /// The errors of PostgreSQL for a name that is not a parameter, a parameter that cannot change now, and a value that the parameter does not take.
    pub fn set(
        &mut self,
        name: &str,
        text: Option<&str>,
        action: Action,
        origin: Origin,
    ) -> Result<(), Error> {
        let Some(parameter) = find(name) else {
            return self.set_placeholder(name, text, action, origin);
        };
        self.allowed(parameter, origin)?;
        if origin == Origin::Statement {
            self.check_definer(name)?;
        }
        if text.is_none() && parameter.has(flag::NO_RESET) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("parameter \"{}\" cannot be reset", parameter.name),
            ));
        }
        let key = lower(parameter.name);
        let value = match text {
            Some(text) => Value {
                setting: self.parse(parameter, text)?,
                source: if origin == Origin::Startup { Source::Client } else { Source::Session },
            },
            None => self.find_slot(key.as_ref()).map_or_else(
                || Value { setting: parameter.boot(), source: Source::Default },
                |slot| slot.reset.clone(),
            ),
        };
        match origin {
            Origin::Startup => self.store_reset(parameter, value),
            Origin::Statement => {
                if let (Setting::Real(seed), "seed") = (&value.setting, parameter.name) {
                    self.seed = Some(*seed);
                }
                self.slot(&key, Some(parameter), parameter.name);
                self.assign(&key, value, action);
                // `set_config_with_handle`: a change of `session_authorization` does `SET ROLE NONE`, or `RESET ROLE` for a reset, with the same action, so the two changes end together.
                if parameter.name == "session_authorization" {
                    self.set("role", text.map(|_| "none"), action, origin)?;
                }
            }
        }
        Ok(())
    }

    /// `SET` for a name that is not a parameter of PostgreSQL: a placeholder for a parameter of an extension, if the name has a dot.
    fn set_placeholder(
        &mut self,
        name: &str,
        text: Option<&str>,
        action: Action,
        origin: Origin,
    ) -> Result<(), Error> {
        placeholder_name(name)?;
        let key = name.to_ascii_lowercase();
        let source = match (text, origin) {
            (None, _) => Source::Default,
            (Some(_), Origin::Startup) => Source::Client,
            (Some(_), Origin::Statement) => Source::Session,
        };
        let value = Value { setting: Setting::String(text.unwrap_or("").to_owned()), source };
        let slot = self.slot(&key, None, name);
        match origin {
            Origin::Startup => {
                slot.current = value.clone();
                slot.reset = value;
            }
            Origin::Statement => self.assign(&key, value, action),
        }
        Ok(())
    }

    /// The check of the context of a parameter in `set_config_with_handle`.
    fn allowed(&self, parameter: &Parameter, origin: Origin) -> Result<(), Error> {
        let name = parameter.name;
        let cannot = |what: &str| {
            Err(Error::new(
                SqlState::CANT_CHANGE_RUNTIME_PARAM,
                format!("parameter \"{name}\" {what}"),
            ))
        };
        let denied = || {
            Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied to set parameter \"{name}\""),
            ))
        };
        match (parameter.context, origin) {
            (Context::Internal, _) => cannot("cannot be changed"),
            (Context::Postmaster, _) => cannot("cannot be changed without restarting the server"),
            (Context::Sighup, _) => cannot("cannot be changed now"),
            (Context::SuperuserBackend | Context::Backend, Origin::Statement) => {
                cannot("cannot be set after connection start")
            }
            (Context::SuperuserBackend | Context::Superuser, _) if !self.superuser => denied(),
            _ => Ok(()),
        }
    }

    /// Reads a value with the type rules and the check hook of the parameter, and refuses a value that rupg does not support (spec/06 section 6.13.2).
    fn parse(&self, parameter: &Parameter, text: &str) -> Result<Setting, Error> {
        let setting = parameter.parse(text)?;
        let (current, reset) = match self.find_slot(lower(parameter.name).as_ref()) {
            Some(slot) => {
                (parameter.show(&slot.current.setting), parameter.show(&slot.reset.setting))
            }
            None => {
                let boot = parameter.show(&parameter.boot());
                (boot.clone(), boot)
            }
        };
        let setting = check::check(parameter, setting, &current, &reset)?;
        let shown = parameter.show(&setting);
        self.check_transaction(parameter, &shown, &current)?;
        if super::refuses(parameter, &shown) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!(
                    "rupg does not support parameter \"{}\" set to \"{shown}\"",
                    parameter.name
                ),
            ));
        }
        Ok(setting)
    }

    /// `check_transaction_isolation`, `check_transaction_deferrable` and `check_transaction_read_only`: after the first snapshot of a transaction or in a subtransaction, its isolation level cannot change, its deferrable mode cannot be set, and it cannot become read-write.
    fn check_transaction(
        &self,
        parameter: &Parameter,
        shown: &str,
        current: &str,
    ) -> Result<(), Error> {
        let (snapshot, sub) = (self.snapshot, self.subtransaction);
        let message = match parameter.name {
            "transaction_isolation" if shown != current && snapshot => {
                "SET TRANSACTION ISOLATION LEVEL must be called before any query"
            }
            "transaction_isolation" if shown != current && sub => {
                "SET TRANSACTION ISOLATION LEVEL must not be called in a subtransaction"
            }
            "transaction_deferrable" if sub => {
                "SET TRANSACTION [NOT] DEFERRABLE cannot be called within a subtransaction"
            }
            "transaction_deferrable" if snapshot => {
                "SET TRANSACTION [NOT] DEFERRABLE must be called before any query"
            }
            "transaction_read_only" if shown == "off" && current == "on" && sub => {
                "cannot set transaction read-write mode inside a read-only transaction"
            }
            "transaction_read_only" if shown == "off" && current == "on" && snapshot => {
                "transaction read-write mode must be set before any query"
            }
            _ => return Ok(()),
        };
        Err(Error::new(SqlState::ACTIVE_SQL_TRANSACTION, message))
    }

    /// `GetTransactionSnapshot` in a transaction: a statement that analyzes or runs a query takes the first snapshot of the transaction, and [`Settings::start_transaction`] clears it.
    pub fn take_snapshot(&mut self) {
        self.snapshot = true;
    }

    /// Makes the slot of a parameter if the session did not change it before.
    fn slot(&mut self, key: &str, parameter: Option<&'static Parameter>, name: &str) -> &mut Slot {
        let slots = Arc::make_mut(&mut self.slots);
        if !slots.contains_key(key) {
            let slot = match self.base.get(key) {
                Some(slot) => slot.clone(),
                None => {
                    let setting =
                        parameter.map_or_else(|| Setting::String(String::new()), Parameter::boot);
                    let value = Value { setting, source: Source::Default };
                    self.count += 1;
                    Slot {
                        parameter,
                        name: name.to_owned(),
                        current: value.clone(),
                        reset: value,
                        save: None,
                        location: None,
                        order: self.count - 1,
                    }
                }
            };
            slots.insert(key.to_owned(), slot);
        }
        slots.get_mut(key).expect("the slot is there")
    }

    /// Sets the current value and the reset value, outside of a transaction.
    fn store_reset(&mut self, parameter: &'static Parameter, value: Value) {
        let key = lower(parameter.name);
        let slot = self.slot(&key, Some(parameter), parameter.name);
        slot.reset = value.clone();
        if slot.current != value {
            slot.current = value;
            self.changed(Some(parameter));
        }
    }

    /// Sets the current value in the current transaction, with the save of `push_old_value`.
    fn assign(&mut self, key: &str, value: Value, action: Action) {
        self.roles_set |= is_role_key(key);
        let slot = owned(&mut self.slots, &self.base, key).expect("the caller made the slot");
        match (&mut slot.save, action) {
            (None, _) => {
                let state = if action == Action::Set { State::Set } else { State::Local };
                slot.save = Some(Save { prior: slot.current.clone(), state });
                self.saved.push(key.to_owned());
            }
            (Some(save), Action::Set) => save.state = State::Set,
            (Some(save), Action::Local) => {
                if matches!(save.state, State::Set) {
                    save.state = State::SetLocal(slot.current.clone());
                }
            }
        }
        let parameter = slot.parameter;
        slot.current = value;
        // `push_old_value`: a `SET` in a function changes the state `GUC_SAVE` of the call to `GUC_SET`, so the value stays after the call.
        if action == Action::Set {
            for frame in &mut self.frames {
                if !frame.kept.iter().any(|kept| kept == key) {
                    frame.kept.push(key.to_owned());
                }
            }
        }
        self.changed(parameter);
    }

    /// `fmgr_security_definer` for a call of a function: `SetUserIdAndSecContext` makes `definer`, the OID and the name of the owner of a `SECURITY DEFINER` function, the current user, then `ProcessGUCArray` with `GUC_ACTION_SAVE` applies each setting of `config`, as `name=value`, as a `SET LOCAL`. Both stay until [`Settings::pop_function`].
    ///
    /// # Errors
    ///
    /// The errors of `SET` for the names and the values. The settings that applied before the error go back.
    pub fn push_function(
        &mut self,
        config: &[String],
        definer: Option<(u32, String)>,
    ) -> Result<(), Error> {
        self.frames.push(Frame { definer, ..Frame::default() });
        for item in config {
            let Some((name, value)) = item.split_once('=') else { continue };
            let key = find(name)
                .map_or_else(|| name.to_ascii_lowercase(), |p| lower(p.name).into_owned());
            let before = self.find_slot(&key).cloned();
            if let Some(frame) = self.frames.last_mut()
                && !frame.saved.iter().any(|(saved, _)| *saved == key)
            {
                frame.saved.push((key, before));
            }
            if let Err(error) = self.set(name, Some(value), Action::Local, Origin::Statement) {
                self.pop_function();
                return Err(error);
            }
        }
        Ok(())
    }

    /// `AtEOXact_GUC` at the end of a call of [`Settings::push_function`]: each setting of the call goes back to its value from before, other than one that a `SET` changed during the call. A placeholder that the call made stays, with its reset value.
    pub fn pop_function(&mut self) {
        let Some(frame) = self.frames.pop() else { return };
        for (key, before) in frame.saved {
            if frame.kept.contains(&key) {
                continue;
            }
            let Some(slot) = owned(&mut self.slots, &self.base, &key) else { continue };
            let now = slot.current.clone();
            match before {
                Some(before) => *slot = before,
                None => {
                    slot.current = slot.reset.clone();
                    slot.save = None;
                }
            }
            if slot.current != now {
                let parameter = slot.parameter;
                self.changed(parameter);
            }
        }
        self.generation += 1;
    }

    /// Notes a change of a current value.
    fn changed(&mut self, parameter: Option<&'static Parameter>) {
        self.generation += 1;
        if let Some(parameter) = parameter
            && parameter.has(flag::REPORT)
            && !self.pending.contains(&parameter.name)
        {
            self.pending.push(parameter.name);
        }
    }

    /// The isolation level, the read only mode and the deferrable mode of the current transaction, which `AND CHAIN` gives to the next transaction as `SaveTransactionCharacteristics` does.
    #[must_use]
    pub fn characteristics(&self) -> Characteristics {
        Characteristics(CHARACTERISTICS.map(|(name, _)| {
            find(name).map_or(Setting::Bool(false), |parameter| self.setting(parameter))
        }))
    }

    /// `StartTransaction`: a new transaction takes `transaction_isolation`, `transaction_read_only` and `transaction_deferrable` from their defaults, or from the transaction before it after `AND CHAIN`. PostgreSQL sets the variables of these parameters directly, so the change is not in the stack of the transaction, the end of the transaction does not restore it, and the source of the value does not change.
    pub fn start_transaction(&mut self, chained: Option<Characteristics>) {
        self.snapshot = false;
        self.subtransaction = false;
        for (i, (name, default)) in CHARACTERISTICS.into_iter().enumerate() {
            let (Some(parameter), Some(default)) = (find(name), find(default)) else { continue };
            let setting = match &chained {
                Some(Characteristics(values)) => values[i].clone(),
                None => self.setting(default),
            };
            if self.setting(parameter) != setting {
                // The source stays, so `pg_settings` shows `session` after a `SET TRANSACTION` that committed, as in PostgreSQL.
                let source = self
                    .find_slot(lower(parameter.name).as_ref())
                    .map_or(Source::Default, |slot| slot.current.source);
                self.store_reset(parameter, Value { setting, source });
            }
        }
    }

    /// `RESET ALL`: the reset value for each parameter that a statement set, other than the ones that `RESET ALL` does not change.
    pub fn reset_all(&mut self) {
        let mut keys: Vec<(usize, String)> = self
            .each_slot()
            .filter(|(_, slot)| {
                slot.current.source == Source::Session
                    && slot.parameter.is_none_or(|parameter| {
                        matches!(parameter.context, Context::Superuser | Context::User)
                            && !parameter.has(flag::NO_RESET_ALL)
                    })
            })
            .map(|(key, slot)| (slot.order, key.clone()))
            .collect();
        keys.sort();
        for (_, key) in keys {
            let Some(reset) = self.find_slot(&key).map(|slot| slot.reset.clone()) else { continue };
            self.assign(&key, reset, Action::Set);
        }
    }

    /// Notes that the transaction has a savepoint or has none, for the checks of `SET TRANSACTION`.
    pub fn set_subtransaction(&mut self, subtransaction: bool) {
        self.subtransaction = subtransaction;
    }

    /// The values at a `SAVEPOINT`, which [`Settings::rollback_to`] brings back. The slots are shared, so this copies no value.
    #[must_use]
    pub fn savepoint(&self) -> Savepoint {
        let read_only = find("transaction_read_only").map(|parameter| self.setting(parameter));
        Savepoint {
            slots: Arc::clone(&self.slots),
            saved: self.saved.clone(),
            read_only: read_only.unwrap_or(Setting::Bool(false)),
        }
    }

    /// `AtEOSubXact_GUC` after `ROLLBACK TO SAVEPOINT` or an error in a subtransaction: each value goes back to its value at the savepoint, and a reported value that changes is reported again.
    pub fn rollback_to(&mut self, savepoint: &Savepoint) {
        let now = std::mem::replace(&mut self.slots, Arc::clone(&savepoint.slots));
        let keys = std::mem::replace(&mut self.saved, savepoint.saved.clone());
        self.generation += 1;
        for key in keys {
            self.roles_set |= is_role_key(&key);
            let Some(slot) = now.get(&key) else { continue };
            if self.find_slot(&key).is_none_or(|old| old.current != slot.current) {
                self.changed(slot.parameter);
            }
        }
    }

    /// `CommitSubTransaction` after `RELEASE SAVEPOINT`: the values of the subtransaction stay, but `transaction_read_only` goes back to its value at the savepoint, as `XactReadOnly` takes `prevXactReadOnly`.
    pub fn release(&mut self, savepoint: &Savepoint) {
        let Some(parameter) = find("transaction_read_only") else { return };
        if self.setting(parameter) == savepoint.read_only {
            return;
        }
        let key = lower(parameter.name);
        let slot = self.slot(&key, Some(parameter), parameter.name);
        slot.current.setting = savepoint.read_only.clone();
        self.changed(Some(parameter));
    }

    /// The end of a transaction: keeps or restores the values that it changed.
    pub fn end(&mut self, commit: bool) {
        for key in std::mem::take(&mut self.saved) {
            self.roles_set |= is_role_key(&key);
            let Some(slot) = owned(&mut self.slots, &self.base, &key) else { continue };
            let Some(save) = slot.save.take() else { continue };
            let restore = match save.state {
                State::Set if commit => None,
                State::SetLocal(masked) if commit => Some(masked),
                _ => Some(save.prior),
            };
            if let Some(value) = restore
                && value != slot.current
            {
                slot.current = value;
                let parameter = slot.parameter;
                self.changed(parameter);
            }
        }
    }

    /// Whether `role` or `session_authorization` got a value since the last call. The call clears it.
    pub fn take_roles_set(&mut self) -> bool {
        std::mem::take(&mut self.roles_set)
    }

    /// The current value of a parameter as `SHOW` gives it, or `None` if there is no parameter or placeholder with this name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<String> {
        match find(name) {
            Some(parameter) => Some(self.shown(parameter)),
            None => self.find_slot(lower(name).as_ref()).map(|slot| match &slot.current.setting {
                Setting::String(text) => text.clone(),
                _ => String::new(),
            }),
        }
    }

    /// The current value of a parameter in its base unit.
    #[must_use]
    pub fn setting(&self, parameter: &'static Parameter) -> Setting {
        if self.slots.is_empty() && self.base.is_empty() {
            return parameter.boot();
        }
        self.find_slot(lower(parameter.name).as_ref())
            .map_or_else(|| parameter.boot(), |slot| slot.current.setting.clone())
    }

    /// Takes the value of the last `SET seed` that `random` did not use yet.
    pub fn take_seed(&mut self) -> Option<f64> {
        self.seed.take()
    }

    /// `SHOW name`: the name of the column and the value.
    ///
    /// # Errors
    ///
    /// 42704 for a name that is not a parameter or a placeholder.
    pub fn show(&self, name: &str) -> Result<(String, String), Error> {
        if let Some(parameter) = find(name) {
            return Ok((parameter.name.to_owned(), self.shown(parameter)));
        }
        match self.find_slot(lower(name).as_ref()) {
            Some(slot) => Ok((slot.name.clone(), self.get(name).unwrap_or_default())),
            None => Err(unrecognized(name)),
        }
    }

    /// `SHOW ALL`: the name, the value and the description of each parameter, in the order of the names without regard to case.
    pub fn show_all(&self) -> impl Iterator<Item = (&'static str, String, &'static str)> + '_ {
        super::all()
            .iter()
            .filter(|parameter| {
                !parameter.has(flag::NO_SHOW_ALL)
                    && (self.superuser || !parameter.has(flag::SUPERUSER_ONLY))
            })
            .map(|parameter| (parameter.name, self.shown(parameter), parameter.short_desc))
    }

    /// The parameters that `pg_show_all_settings` lists, which are those of `SHOW ALL` in the same order, each with the columns that [`Current`] holds, as `GetConfigOptionValues` gives them.
    pub fn show_all_current(&self) -> impl Iterator<Item = Current> + '_ {
        super::all()
            .iter()
            .filter(|parameter| {
                !parameter.has(flag::NO_SHOW_ALL)
                    && (self.superuser || !parameter.has(flag::SUPERUSER_ONLY))
            })
            .map(|parameter| self.current(parameter))
    }

    /// The row of `pg_settings` for one parameter.
    fn current(&self, parameter: &'static Parameter) -> Current {
        let slot = self.find_slot(lower(parameter.name).as_ref());
        let value = slot.map_or_else(|| parameter.boot(), |slot| slot.current.setting.clone());
        // `ShowGUCOption` without the units: a number in the base unit, unless a show hook gives the text.
        let setting = match value {
            _ if tcp_index(parameter).is_some() => self.shown(parameter),
            Setting::Int(_) | Setting::Real(_) if !parameter.mode() => parameter.setting(&value),
            _ => self.shown(parameter),
        };
        let source = match slot.map_or(Source::Default, |slot| slot.current.source) {
            // `InitializeGUCOptions` sets the modes of the transaction with `PGC_S_OVERRIDE`.
            Source::Default if CHARACTERISTICS.iter().any(|(name, _)| *name == parameter.name) => {
                "override"
            }
            source => source.name(),
        };
        let reset = match slot {
            Some(slot) => Some(parameter.setting(&slot.reset.setting)),
            None => parameter.boot_text(),
        };
        // `GetConfigOptionValues` shows the line only to a role that can read all the settings.
        let location = slot
            .filter(|slot| slot.current.source == Source::File && self.superuser)
            .and_then(|slot| slot.location.as_ref())
            .map(|(file, line)| (file.to_string(), *line));
        Current { parameter, setting, source, reset, location }
    }

    /// The value of a parameter as `SHOW` gives it, with the show hooks of PostgreSQL.
    fn shown(&self, parameter: &'static Parameter) -> String {
        if let Some(index) = tcp_index(parameter) {
            return self.tcp_shown(parameter, index);
        }
        let value = parameter.show(&self.setting(parameter));
        match parameter.name {
            "archive_command" if self.get("archive_mode").as_deref() == Some("off") => {
                "(disabled)".to_owned()
            }
            "timing_clock_source" if value == "auto" => {
                if rupg_platform::tsc_is_default() { "auto (tsc)" } else { "auto (system)" }
                    .to_owned()
            }
            "seed" => "unavailable".to_owned(),
            _ => value,
        }
    }

    /// The show hooks of the TCP parameters, as `pq_getkeepalivesidle` and the others give the value: 0 for a Unix socket and for an option that the system does not have, the value of the parameter when it is not 0, and else the value that the system gave to the socket.
    fn tcp_shown(&self, parameter: &'static Parameter, index: usize) -> String {
        let Some(values) = self.socket else { return "0".to_owned() };
        if !TcpOption::ALL[index].supported() {
            return "0".to_owned();
        }
        match self.setting(parameter) {
            Setting::Int(value) if value != 0 => value.to_string(),
            _ => values[index].to_string(),
        }
    }

    /// The `ParameterStatus` messages of the start of a session, in the order of PostgreSQL.
    pub fn startup_reports(&mut self) -> Vec<(&'static str, String)> {
        static PARAMETERS: LazyLock<Vec<&'static Parameter>> =
            LazyLock::new(|| STARTUP_REPORTS.iter().filter_map(|name| find(name)).collect());
        self.pending.clear();
        PARAMETERS
            .iter()
            .map(|&parameter| {
                let value = self.shown(parameter);
                Arc::make_mut(&mut self.reported).insert(parameter.name, value.clone());
                (parameter.name, value)
            })
            .collect()
    }

    /// The `ParameterStatus` messages for the reported values that changed since the last report, in the order of `ReportChangedGUCOptions`, which takes the last change first.
    pub fn reports(&mut self) -> Vec<(&'static str, String)> {
        let mut reports = Vec::new();
        for name in std::mem::take(&mut self.pending).into_iter().rev() {
            let Some(parameter) = find(name) else { continue };
            let value = self.shown(parameter);
            if self.reported.get(name) != Some(&value) {
                Arc::make_mut(&mut self.reported).insert(name, value.clone());
                reports.push((name, value));
            }
        }
        reports
    }
}

/// The place of a TCP parameter in [`TcpOption::ALL`], or `None` for another parameter.
fn tcp_index(parameter: &Parameter) -> Option<usize> {
    TcpOption::ALL.iter().position(|option| option.parameter() == parameter.name)
}

/// The check of a name that is not a parameter of PostgreSQL, for a placeholder: it needs a dot, the form of a custom name, and a prefix that is not reserved.
fn placeholder_name(name: &str) -> Result<(), Error> {
    if !name.contains('.') {
        return Err(unrecognized(name));
    }
    if !valid_custom_name(name) {
        return Err(Error::new(
            SqlState::INVALID_NAME,
            format!("invalid configuration parameter name \"{name}\""),
        )
        .with_detail(
            "Custom parameter names must be two or more simple identifiers separated by dots.",
        ));
    }
    let prefix = name.split('.').next().unwrap_or("");
    if prefix.eq_ignore_ascii_case(RESERVED_PREFIX) {
        return Err(Error::new(
            SqlState::INVALID_NAME,
            format!("invalid configuration parameter name \"{name}\""),
        )
        .with_detail(format!("\"{RESERVED_PREFIX}\" is a reserved prefix.")));
    }
    Ok(())
}

/// The error for a name that is not a parameter.
fn unrecognized(name: &str) -> Error {
    Error::new(
        SqlState::UNDEFINED_OBJECT,
        format!("unrecognized configuration parameter \"{name}\""),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(settings: &mut Settings, name: &str, text: &str) -> Result<(), Error> {
        settings.set(name, Some(text), Action::Set, Origin::Statement)
    }

    fn local(settings: &mut Settings, name: &str, text: &str) {
        settings.set(name, Some(text), Action::Local, Origin::Statement).unwrap();
    }

    fn startup() -> Settings {
        let mut settings = Settings::new(true);
        settings.set_internal("TimeZone", "UTC").unwrap();
        settings.set("application_name", Some("probe"), Action::Set, Origin::Startup).unwrap();
        settings.startup_reports();
        settings
    }

    #[test]
    fn a_transaction_takes_its_characteristics_from_the_defaults() {
        use super::super::{EnumOption, Kind};
        let options = |o: &[EnumOption]| o.iter().map(|o| (o.name, o.value)).collect::<Vec<_>>();
        for (name, default) in CHARACTERISTICS {
            let (parameter, default) = (find(name).unwrap(), find(default).unwrap());
            match (parameter.kind, default.kind) {
                (Kind::Enum { options: a, .. }, Kind::Enum { options: b, .. }) => {
                    assert_eq!(options(a), options(b), "{name}");
                }
                (Kind::Bool { .. }, Kind::Bool { .. }) => {}
                _ => panic!("{name} and {} have different types", default.name),
            }
        }
        let mut s = startup();
        set(&mut s, "default_transaction_isolation", "repeatable read").unwrap();
        s.end(true);
        assert_eq!(s.get("transaction_isolation").unwrap(), "read committed");
        s.start_transaction(None);
        assert_eq!(s.get("transaction_isolation").unwrap(), "repeatable read");
        // BEGIN ISOLATION LEVEL sets the value for the transaction only, and AND CHAIN keeps it.
        local(&mut s, "transaction_isolation", "read uncommitted");
        let kept = s.characteristics();
        s.end(true);
        assert_eq!(s.get("transaction_isolation").unwrap(), "repeatable read");
        s.start_transaction(Some(kept));
        assert_eq!(s.get("transaction_isolation").unwrap(), "read uncommitted");
        s.end(false);
        assert_eq!(s.get("transaction_isolation").unwrap(), "read uncommitted");
    }

    #[test]
    fn transactions_keep_or_restore_values() {
        let mut s = startup();
        set(&mut s, "timezone", "Asia/Tokyo").unwrap();
        s.end(true);
        assert_eq!(s.reports(), [("TimeZone", "Asia/Tokyo".to_owned())]);
        set(&mut s, "timezone", "Europe/Paris").unwrap();
        s.end(false);
        assert_eq!(s.get("TimeZone").unwrap(), "Asia/Tokyo");
        assert!(s.reports().is_empty(), "the value is the one that the client has");

        local(&mut s, "datestyle", "sql, dmy");
        assert_eq!(s.get("datestyle").unwrap(), "SQL, DMY");
        set(&mut s, "datestyle", "german").unwrap();
        local(&mut s, "datestyle", "iso");
        assert_eq!(s.get("datestyle").unwrap(), "ISO, DMY");
        s.end(true);
        assert_eq!(s.get("datestyle").unwrap(), "German, DMY");
        set(&mut s, "datestyle", "ymd").unwrap();
        assert_eq!(s.get("datestyle").unwrap(), "German, YMD");
        s.set("datestyle", None, Action::Set, Origin::Statement).unwrap();
        assert_eq!(s.get("datestyle").unwrap(), "ISO, MDY");
    }

    #[test]
    fn reports_take_the_last_change_first() {
        let mut s = startup();
        set(&mut s, "work_mem", "64MB").unwrap();
        set(&mut s, "application_name", "a").unwrap();
        set(&mut s, "IntervalStyle", "iso_8601").unwrap();
        s.end(true);
        assert_eq!(
            s.reports(),
            [("IntervalStyle", "iso_8601".to_owned()), ("application_name", "a".to_owned())]
        );
        s.reset_all();
        s.end(true);
        assert_eq!(s.get("work_mem").unwrap(), "4MB");
        assert_eq!(s.get("application_name").unwrap(), "probe");
        assert_eq!(s.reports().len(), 2);
    }

    #[test]
    fn the_compat_version_is_19_and_is_not_in_show_all() {
        let mut s = Settings::new(true);
        assert_eq!(s.show("rupg.compat_version").unwrap().1, "19");
        s.set("rupg.compat_version", Some("19"), Action::Set, Origin::Startup).unwrap();
        assert_eq!(s.show("RUPG.COMPAT_VERSION").unwrap().1, "19");
        let state = |r: Result<(), Error>| r.unwrap_err().state().as_str().to_owned();
        let at_startup = |s: &mut Settings, v| {
            s.set("rupg.compat_version", Some(v), Action::Set, Origin::Startup)
        };
        assert_eq!(state(at_startup(&mut s, "16")), "0A000");
        assert_eq!(state(at_startup(&mut s, "13")), "22023");
        assert!(super::super::all().iter().all(|p| !p.name.starts_with("rupg.")));
    }

    #[test]
    fn the_version_of_rupg_is_internal() {
        let mut s = startup();
        let version = env!("CARGO_PKG_VERSION");
        assert_eq!(
            s.show("RUPG.VERSION").unwrap(),
            ("rupg.version".to_owned(), version.to_owned())
        );
        let e = set(&mut s, "rupg.version", "1.0.0").unwrap_err();
        assert_eq!(e.state().as_str(), "55P02");
        assert_eq!(e.message(), "parameter \"rupg.version\" cannot be changed");
        assert_eq!(s.show("rupg.version").unwrap().1, version);
        assert!(s.show_all().all(|p| p.0 != "rupg.version"));
    }

    #[test]
    fn placeholders_and_errors() {
        let mut s = startup();
        set(&mut s, "myapp.user_id", "42").unwrap();
        s.end(true);
        assert_eq!(s.show("MYAPP.USER_ID").unwrap(), ("myapp.user_id".to_owned(), "42".to_owned()));
        set(&mut s, "myapp.other", "x").unwrap();
        s.end(false);
        assert_eq!(s.show("myapp.other").unwrap().1, "");
        let state = |r: Result<(), Error>| r.unwrap_err().state().as_str().to_owned();
        assert_eq!(s.show("myapp.none").unwrap_err().state(), SqlState::UNDEFINED_OBJECT);
        assert_eq!(state(set(&mut s, "nodots", "1")), "42704");
        assert_eq!(state(set(&mut s, "rupg.x", "1")), "42602");
        assert_eq!(state(set(&mut s, "rupg.compat_version", "19")), "55P02");
        assert_eq!(state(set(&mut s, "server_version_num", "1")), "55P02");
        assert_eq!(state(set(&mut s, "log_connections", "on")), "55P02");
        assert_eq!(state(set(&mut s, "standard_conforming_strings", "off")), "0A000");
        assert_eq!(
            state(s.set("transaction_isolation", None, Action::Set, Origin::Statement)),
            "0A000"
        );
        assert_eq!(state(s.set("is_superuser", None, Action::Set, Origin::Statement)), "55P02");
        assert_eq!(
            state(Settings::new(false).set(
                "lc_messages",
                Some("C"),
                Action::Set,
                Origin::Statement
            )),
            "42501"
        );
        s.reset_all();
        assert_eq!(s.show("myapp.user_id").unwrap().1, "");
    }

    #[test]
    fn flatten_quotes_list_items() {
        let args = [
            Arg::String("My Schema".into()),
            Arg::String("public".into()),
            Arg::String("x y".into()),
            Arg::String("z".into()),
        ];
        assert_eq!(flatten("search_path", &args).unwrap(), "\"My Schema\", public, \"x y\", z");
        assert_eq!(flatten("search_path", &[Arg::String(String::new())]).unwrap(), "\"\"");
        let two = [Arg::Integer("1".into()), Arg::Integer("2".into())];
        assert_eq!(
            flatten("extra_float_digits", &two).unwrap_err().message(),
            "SET extra_float_digits takes only one argument"
        );
    }

    #[test]
    fn the_tcp_parameters_show_the_socket() {
        let mut s = Settings::new(true);
        assert_eq!(s.get("tcp_keepalives_idle").as_deref(), Some("0"), "no socket");
        s.set_socket([7200, 75, 9, -1]);
        assert_eq!(s.get("tcp_keepalives_idle").as_deref(), Some("7200"));
        // macOS does not have `TCP_USER_TIMEOUT`, so it shows 0 for each value.
        let macos = cfg!(target_os = "macos");
        assert_eq!(s.get("tcp_user_timeout").as_deref(), Some(if macos { "0" } else { "-1" }));
        s.set("tcp_keepalives_idle", Some("60"), Action::Set, Origin::Statement).unwrap();
        assert_eq!(s.get("tcp_keepalives_idle").as_deref(), Some("60"));
        assert_eq!(s.tcp_values(), [60, 0, 0, 0]);
        s.set("tcp_user_timeout", Some("5000"), Action::Set, Origin::Statement).unwrap();
        assert_eq!(s.get("tcp_user_timeout").as_deref(), Some(if macos { "0" } else { "5000" }));
    }

    #[test]
    fn show_all_has_the_hooks() {
        let s = startup();
        let all: Vec<_> = s.show_all().collect();
        assert!(
            all.iter().any(|(name, value, _)| *name == "archive_command" && value == "(disabled)")
        );
        assert!(all.windows(2).all(|w| w[0].0.to_ascii_lowercase() < w[1].0.to_ascii_lowercase()));
    }

    #[test]
    fn file_values_go_under_arguments_and_clients() {
        let mut s = Settings::new(true);
        assert!(s.set_file("work_mem", Some("8MB")).unwrap());
        assert_eq!(s.get("work_mem").unwrap(), "8MB");
        s.set_argument("work_mem", "16MB").unwrap();
        assert!(!s.set_file("work_mem", Some("32MB")).unwrap(), "the command line wins");
        assert_eq!(s.get("work_mem").unwrap(), "16MB");

        assert!(s.set_file("DateStyle", Some("sql, dmy")).unwrap());
        s.set("datestyle", Some("German"), Action::Set, Origin::Startup).unwrap();
        assert!(!s.set_file("datestyle", Some("iso, ymd")).unwrap());
        assert_eq!(s.get("DateStyle").unwrap(), "German, DMY");

        set(&mut s, "statement_timeout", "5s").unwrap();
        s.end(true);
        assert!(!s.set_file("statement_timeout", Some("7s")).unwrap());
        assert_eq!(s.get("statement_timeout").unwrap(), "5s");
        s.set("statement_timeout", None, Action::Set, Origin::Statement).unwrap();
        assert_eq!(s.get("statement_timeout").unwrap(), "7s", "RESET goes to the file value");
    }

    #[test]
    fn file_values_leave_and_go_under_transactions() {
        let mut s = Settings::new(true);
        s.set_file("lock_timeout", Some("3s")).unwrap();
        assert!(s.set_file("lock_timeout", None).unwrap());
        assert_eq!(s.get("lock_timeout").unwrap(), "0");

        local(&mut s, "lock_timeout", "1s");
        assert!(!s.set_file("lock_timeout", Some("2s")).unwrap());
        s.end(false);
        assert_eq!(s.get("lock_timeout").unwrap(), "2s");

        assert!(s.set_file("my.option", Some("x")).unwrap());
        assert_eq!(s.get("my.option").unwrap(), "x");
        assert!(s.set_file("nodot", Some("x")).is_err());
        assert!(s.set_file("rupg.option", Some("x")).is_err());
        assert!(s.set_file("work_mem", Some("1kB")).is_err());
    }

    #[test]
    fn shared_values_stay_the_same_for_the_other_copies() {
        let mut server = Settings::new(true);
        server.set_file("work_mem", Some("8MB")).unwrap();
        server.set_file("my.option", Some("x")).unwrap();
        server.set_argument("lock_timeout", "2s").unwrap();
        server.share();
        assert!(server.slots.is_empty());
        let mut one = server.clone();
        let two = server.clone();
        assert!(Arc::ptr_eq(&one.base, &two.base));

        set(&mut one, "work_mem", "64MB").unwrap();
        set(&mut one, "my.option", "y").unwrap();
        set(&mut one, "statement_timeout", "5s").unwrap();
        one.end(true);
        assert_eq!(one.slots.len(), 3, "only the changed slots are copied");
        assert_eq!(one.get("work_mem").unwrap(), "64MB");
        assert_eq!(two.get("work_mem").unwrap(), "8MB");
        assert_eq!(two.get("my.option").unwrap(), "x");
        assert_eq!(one.file_names(), ["work_mem", "my.option"]);

        set(&mut one, "lock_timeout", "9s").unwrap();
        one.end(false);
        assert_eq!(one.get("lock_timeout").unwrap(), "2s");
        one.reset_all();
        one.end(true);
        assert_eq!(one.get("work_mem").unwrap(), "8MB", "RESET ALL goes to the shared value");
        assert_eq!(one.get("my.option").unwrap(), "x");
        assert_eq!(one.get("statement_timeout").unwrap(), "0");
        assert_eq!(two.slots.len(), 0);
    }

    #[test]
    fn refused_values() {
        let mut s = startup();
        let message = |r: Result<(), Error>| {
            let error = r.unwrap_err();
            format!("{} {}", error.state().as_str(), error.message())
        };
        assert_eq!(
            message(set(&mut s, "default_transaction_isolation", "SERIALIZABLE")),
            "0A000 rupg does not support parameter \"default_transaction_isolation\" set to \"serializable\""
        );
        assert_eq!(
            message(set(&mut s, "transaction_isolation", "serializable")),
            "0A000 rupg does not support parameter \"transaction_isolation\" set to \"serializable\""
        );
        assert_eq!(state_of(set(&mut s, "transaction_isolation", "bogus")), "22023");
        set(&mut s, "session_replication_role", "local").unwrap();
        set(&mut s, "default_transaction_isolation", "repeatable read").unwrap();
        s.end(true);
        assert_eq!(s.get("default_transaction_isolation").unwrap(), "repeatable read");
        assert_eq!(state_of(s.set_argument("ssl_sni", "on")), "0A000");
        assert_eq!(state_of(s.set_file("ssl_sni", Some("on")).map(drop)), "0A000");
        s.set_argument("ssl_sni", "off").unwrap();
    }

    fn state_of(r: Result<(), Error>) -> String {
        r.unwrap_err().state().as_str().to_owned()
    }
}
