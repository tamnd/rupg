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

/// Where a value came from, which `RESET ALL` reads to find the values that a statement set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The boot value or a value that the server set.
    Default,
    /// A value of `postgresql.conf`, which a reload of the file can change.
    File,
    /// A value of the command line of the server.
    Argument,
    /// A value of the startup packet.
    Client,
    /// A value that a statement set.
    Session,
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
    /// The number of slots before this one. `RESET ALL` goes in this order, as PostgreSQL goes over the parameters in the order that they left their default values.
    order: usize,
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
    /// The names of the reported parameters that changed since the last report, the first one first.
    pending: Vec<&'static str>,
    /// The value of each reported parameter that the client last got.
    reported: Arc<HashMap<&'static str, String>>,
    generation: u64,
    superuser: bool,
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
            pending: Vec::new(),
            reported: Arc::default(),
            generation: 0,
            superuser,
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
                self.slot(&key, Some(parameter), parameter.name);
                self.assign(&key, value, action);
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

    /// Reads a value with the type rules and the check hook of the parameter.
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
        check::check(parameter, setting, &current, &reset)
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
        self.changed(parameter);
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

    /// `StartTransaction`: a new transaction takes `transaction_isolation`, `transaction_read_only` and `transaction_deferrable` from their defaults, or from the transaction before it after `AND CHAIN`. PostgreSQL sets the variables of these parameters directly, so the change is not in the stack of the transaction and the end of the transaction does not restore it.
    pub fn start_transaction(&mut self, chained: Option<Characteristics>) {
        for (i, (name, default)) in CHARACTERISTICS.into_iter().enumerate() {
            let (Some(parameter), Some(default)) = (find(name), find(default)) else { continue };
            let setting = match &chained {
                Some(Characteristics(values)) => values[i].clone(),
                None => self.setting(default),
            };
            if self.setting(parameter) != setting {
                self.store_reset(parameter, Value { setting, source: Source::Default });
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

    /// The end of a transaction: keeps or restores the values that it changed.
    pub fn end(&mut self, commit: bool) {
        for key in std::mem::take(&mut self.saved) {
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

    /// The value of a parameter as `SHOW` gives it, with the show hooks of PostgreSQL.
    fn shown(&self, parameter: &'static Parameter) -> String {
        let value = parameter.show(&self.setting(parameter));
        match parameter.name {
            "archive_command" if self.get("archive_mode").as_deref() == Some("off") => {
                "(disabled)".to_owned()
            }
            "timing_clock_source" if value == "auto" => "auto (system)".to_owned(),
            _ => value,
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
        set(&mut s, "default_transaction_isolation", "serializable").unwrap();
        s.end(true);
        assert_eq!(s.get("transaction_isolation").unwrap(), "read committed");
        s.start_transaction(None);
        assert_eq!(s.get("transaction_isolation").unwrap(), "serializable");
        // BEGIN ISOLATION LEVEL sets the value for the transaction only, and AND CHAIN keeps it.
        local(&mut s, "transaction_isolation", "repeatable read");
        let kept = s.characteristics();
        s.end(true);
        assert_eq!(s.get("transaction_isolation").unwrap(), "serializable");
        s.start_transaction(Some(kept));
        assert_eq!(s.get("transaction_isolation").unwrap(), "repeatable read");
        s.end(false);
        assert_eq!(s.get("transaction_isolation").unwrap(), "repeatable read");
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
}
