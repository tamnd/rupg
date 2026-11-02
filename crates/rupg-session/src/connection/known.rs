//! The values of the settings that each statement reads, kept for one [`Settings::generation`], so that the path of a statement does not look up the settings again while no value changed.

use crate::guc::{self, Setting, Settings};

/// The value of `off` in `synchronous_commit_options`.
const SYNCHRONOUS_OFF: u8 = 4;

/// The settings that the start and the end of each statement read.
#[derive(Debug, Default)]
pub(super) struct Known {
    generation: Option<u64>,
    /// `application_name`, cut to the bytes that the table of the sessions keeps.
    pub(super) application: String,
    /// `track_activities`.
    pub(super) tracked: bool,
    /// `track_counts`.
    pub(super) counts: bool,
    /// True when `compute_query_id` is `on` or `regress`.
    pub(super) query_id: bool,
    /// `track_activity_query_size - 1`, the most bytes of the text of a statement in the table.
    pub(super) query_bytes: usize,
    /// `statement_timeout` in milliseconds, or 0.
    pub(super) timeout: i64,
    /// False for `synchronous_commit = off`.
    pub(super) synchronous: bool,
    /// True when the client encoding is not `SQL_ASCII`.
    pub(super) utf8: bool,
    /// `plan_cache_mode`.
    pub(super) plan_cache_mode: Option<String>,
}

impl Known {
    /// The values for the current generation of `settings`.
    pub(super) fn of(&mut self, settings: &Settings) -> &Known {
        if self.generation != Some(settings.generation()) {
            *self = Known::read(settings);
        }
        self
    }

    fn read(settings: &Settings) -> Known {
        let int = |name| match guc::find(name).map(|p| settings.setting(p)) {
            Some(Setting::Int(value)) => Some(value),
            _ => None,
        };
        let off = |parameter| settings.get_of(parameter).as_deref() == Some("off");
        Known {
            generation: Some(settings.generation()),
            application: super::activity::application_name(settings),
            tracked: !off(guc::known!("track_activities")),
            counts: !off(guc::known!("track_counts")),
            query_id: matches!(
                settings.get_of(guc::known!("compute_query_id")).as_deref(),
                Some("on" | "regress")
            ),
            query_bytes: int("track_activity_query_size")
                .map_or(1023, |size| usize::try_from(size).map_or(0, |s| s.saturating_sub(1))),
            timeout: int("statement_timeout").filter(|&ms| ms > 0).map_or(0, i64::from),
            synchronous: !matches!(
                guc::find("synchronous_commit").map(|p| settings.setting(p)),
                Some(Setting::Enum(value)) if value == SYNCHRONOUS_OFF
            ),
            utf8: settings
                .get_of(guc::known!("client_encoding"))
                .is_none_or(|name| name != "SQL_ASCII"),
            plan_cache_mode: settings.get_of(guc::known!("plan_cache_mode")),
        }
    }
}
