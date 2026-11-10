//! Regression tests for issue #8: completing a recurring task scheduled the
//! next occurrence a day early in timezones ahead of UTC.
//!
//! chrono reads the local timezone from `TZ` and caches it, so setting `TZ`
//! inside a running test is unreliable. Instead, `next_occurrence_in_each_timezone`
//! re-runs this test binary once per timezone with `TZ` set in the child's
//! environment, and the ignored `child_assertions` test does the actual
//! checks there.

use std::process::Command;

use chrono::{Local, NaiveDate, Offset, TimeZone};
use tasknotes_tui::recurrence::{complete, RecurrenceInput};

const CHILD_ENV: &str = "TASKNOTES_TZ_TEST_CHILD";
const EXPECTED_OFFSET_ENV: &str = "TASKNOTES_TZ_TEST_EXPECTED_OFFSET";

/// (TZ value, UTC offset in seconds on 2026-11-15)
const TIMEZONES: &[(&str, i32)] = &[
    ("UTC", 0),
    ("Asia/Taipei", 8 * 3600),
    ("America/Los_Angeles", -8 * 3600),
];

#[test]
fn next_occurrence_in_each_timezone() {
    let exe = std::env::current_exe().expect("test binary path");
    for (tz, offset) in TIMEZONES {
        let output = Command::new(&exe)
            .args(["child_assertions", "--exact", "--ignored", "--nocapture"])
            .env("TZ", tz)
            .env(CHILD_ENV, "1")
            .env(EXPECTED_OFFSET_ENV, offset.to_string())
            .output()
            .expect("run child test process");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "assertions failed with TZ={tz}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
        );
        assert!(
            stdout.contains("1 passed"),
            "child test did not run with TZ={tz}\n{stdout}"
        );
    }
}

#[test]
#[ignore = "run by next_occurrence_in_each_timezone with TZ set"]
fn child_assertions() {
    if std::env::var(CHILD_ENV).is_err() {
        return;
    }
    let tz = std::env::var("TZ").unwrap_or_default();

    // Make sure the timezone actually took effect, so the test cannot pass
    // vacuously in UTC (for example if tzdata is missing).
    let expected_offset: i32 = std::env::var(EXPECTED_OFFSET_ENV)
        .expect("expected offset")
        .parse()
        .expect("numeric offset");
    let probe = NaiveDate::from_ymd_opt(2026, 11, 15)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    let actual_offset = Local
        .from_local_datetime(&probe)
        .single()
        .expect("unambiguous local time")
        .offset()
        .fix()
        .local_minus_utc();
    assert_eq!(
        actual_offset, expected_offset,
        "TZ={tz} did not take effect"
    );

    // Date-only DTSTART: the next Sunday must stay a Sunday.
    let result = complete(
        &input("DTSTART:20261108;FREQ=WEEKLY;BYDAY=SU", "2026-11-08"),
        "2026-11-08",
    )
    .expect("complete date-only task");
    assert_eq!(
        result["nextScheduled"], "2026-11-15",
        "date-only DTSTART with TZ={tz}: {result}"
    );

    // Date-only DTSTART added from `scheduled` (rule without DTSTART).
    let result = complete(&input("FREQ=WEEKLY;BYDAY=SU", "2026-11-08"), "2026-11-08")
        .expect("complete task without DTSTART");
    assert_eq!(
        result["nextScheduled"], "2026-11-15",
        "rule without DTSTART with TZ={tz}: {result}"
    );

    // UTC datetime DTSTART: behaviour must be unchanged.
    let result = complete(
        &input(
            "DTSTART:20261108T020000Z;FREQ=WEEKLY;BYDAY=SU",
            "2026-11-08T02:00:00Z",
        ),
        "2026-11-08",
    )
    .expect("complete UTC datetime task");
    assert_eq!(
        result["nextScheduled"], "2026-11-15T02:00:00Z",
        "UTC DTSTART with TZ={tz}: {result}"
    );
}

fn input(recurrence: &str, scheduled: &str) -> RecurrenceInput {
    RecurrenceInput {
        recurrence: recurrence.to_string(),
        recurrence_anchor: "scheduled".to_string(),
        scheduled: Some(scheduled.to_string()),
        due: None,
        date_created: None,
        complete_instances: Vec::new(),
        skipped_instances: Vec::new(),
    }
}
