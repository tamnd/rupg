//! A session for the tests of the kernels: the default settings, and a map of the settings that a kernel changes.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use rupg_common::Result;
use rupg_func::{Call, Kernel, Session};
use rupg_types::{ByteaOutput, DateFormat, FixedZone, IntervalStyle, TimeZone, Value};

/// A session in UTC with the default settings.
#[derive(Default)]
pub(crate) struct TestSession {
    settings: RefCell<HashMap<String, String>>,
}

impl TestSession {
    /// The result of `kernel` for `args` of the types `types`.
    pub(crate) fn call(
        &self,
        kernel: Kernel,
        types: &[u32],
        ret: u32,
        args: &[Value],
    ) -> Result<Value> {
        let call =
            Call { session: self, args: types, ret, variadic: false, want: None, collation: 0 };
        kernel(&call, args)
    }
}

impl Session for TestSession {
    fn date_format(&self) -> DateFormat {
        DateFormat::ISO_MDY
    }
    fn interval_style(&self) -> IntervalStyle {
        IntervalStyle::Postgres
    }
    fn zone(&self) -> Result<Rc<dyn TimeZone>> {
        Ok(Rc::new(FixedZone::utc()) as Rc<dyn TimeZone>)
    }
    fn extra_float_digits(&self) -> i32 {
        1
    }
    fn bytea_output(&self) -> ByteaOutput {
        ByteaOutput::Hex
    }
    fn array_nulls(&self) -> bool {
        true
    }
    fn transaction_start(&self) -> i64 {
        0
    }
    fn statement_start(&self) -> i64 {
        0
    }
    fn clock(&self) -> i64 {
        0
    }
    fn user(&self) -> Cow<'_, str> {
        Cow::Borrowed("postgres")
    }
    fn session_user(&self) -> &str {
        "postgres"
    }
    fn database(&self) -> &str {
        "postgres"
    }
    fn schemas(&self) -> Vec<String> {
        vec!["public".to_string()]
    }
    fn backend_pid(&self) -> i32 {
        1
    }
    fn version(&self) -> String {
        "PostgreSQL 19.0".to_string()
    }
    fn setting(&self, name: &str) -> Option<String> {
        self.settings.borrow().get(name).cloned()
    }
    fn set_setting(&self, name: &str, value: Option<&str>, _: bool) -> Result<String> {
        let value = value.unwrap_or_default().to_string();
        self.settings.borrow_mut().insert(name.to_string(), value.clone());
        Ok(value)
    }
}
