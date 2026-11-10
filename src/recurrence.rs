use chrono::{DateTime, Datelike, NaiveDate, Utc};
use rrule::RRuleSet;
use serde_json::{json, Value};

use crate::date::{canonical_date, get_date_part};

#[derive(Debug, Clone)]
pub struct RecurrenceInput {
    pub recurrence: String,
    pub recurrence_anchor: String,
    pub scheduled: Option<String>,
    pub due: Option<String>,
    pub date_created: Option<String>,
    pub complete_instances: Vec<String>,
    pub skipped_instances: Vec<String>,
}

pub fn complete(input: &RecurrenceInput, completion_date: &str) -> anyhow::Result<Value> {
    let completion_date = canonical_date(completion_date)?;
    let mut complete_instances = input.complete_instances.clone();
    if !complete_instances
        .iter()
        .any(|entry| entry == &completion_date)
    {
        complete_instances.push(completion_date.clone());
    }
    let skipped_instances: Vec<String> = input
        .skipped_instances
        .iter()
        .filter(|entry| *entry != &completion_date)
        .cloned()
        .collect();

    let schedule = recalculate_internal(
        input,
        &completion_date,
        Some(&completion_date),
        &complete_instances,
        &skipped_instances,
    )?;

    Ok(json!({
        "updatedRecurrence": schedule.updated_recurrence,
        "nextScheduled": schedule.next_scheduled,
        "nextDue": schedule.next_due,
        "completeInstances": complete_instances,
        "skippedInstances": skipped_instances,
    }))
}

pub fn recalculate(input: &RecurrenceInput, reference_date: &str) -> anyhow::Result<Value> {
    let schedule = recalculate_internal(
        input,
        reference_date,
        None,
        &input.complete_instances,
        &input.skipped_instances,
    )?;

    Ok(json!({
        "updatedRecurrence": schedule.updated_recurrence,
        "nextScheduled": schedule.next_scheduled,
        "nextDue": schedule.next_due,
    }))
}

struct ScheduleResult {
    updated_recurrence: String,
    next_scheduled: Option<String>,
    next_due: Option<String>,
}

fn recalculate_internal(
    input: &RecurrenceInput,
    reference_date: &str,
    completion_anchor_date: Option<&str>,
    complete_instances: &[String],
    skipped_instances: &[String],
) -> anyhow::Result<ScheduleResult> {
    let anchor = if input.recurrence_anchor == "completion" {
        "completion"
    } else {
        "scheduled"
    };
    let source_date = input
        .scheduled
        .as_deref()
        .or(input.date_created.as_deref())
        .unwrap_or(reference_date);

    let mut updated_recurrence = input.recurrence.clone();
    if anchor == "completion" {
        let anchor_date = completion_anchor_date.unwrap_or(reference_date);
        updated_recurrence = update_dtstart_in_recurrence_rule(&updated_recurrence, anchor_date)
            .unwrap_or(updated_recurrence);
    } else {
        updated_recurrence = add_dtstart_to_recurrence_rule(&updated_recurrence, source_date)
            .unwrap_or(updated_recurrence);
    }

    let Some(reference_dt) = parse_date_string(reference_date) else {
        return Ok(ScheduleResult {
            updated_recurrence,
            next_scheduled: None,
            next_due: None,
        });
    };
    let completion_day = parse_date_string(reference_date);
    let mut processed_dates = std::collections::HashSet::new();
    for entry in complete_instances.iter().chain(skipped_instances.iter()) {
        processed_dates.insert(get_date_part(entry));
    }
    processed_dates.insert(format_date_utc(reference_dt));

    let mut next_occurrence =
        get_next_occurrence_date(&updated_recurrence, source_date, reference_dt, true)?;

    if let Some(completion_day) = completion_day {
        let mut guard = 0;
        while let Some(next) = next_occurrence {
            if next >= completion_day || guard >= 1000 {
                next_occurrence = Some(next);
                break;
            }
            next_occurrence =
                get_next_occurrence_date(&updated_recurrence, source_date, next, false)?;
            guard += 1;
        }
    }

    let mut processed_guard = 0;
    while let Some(next) = next_occurrence {
        let date_str = format_date_utc(next);
        if !processed_dates.contains(&date_str) || processed_guard >= 1000 {
            next_occurrence = Some(next);
            break;
        }
        next_occurrence = get_next_occurrence_date(&updated_recurrence, source_date, next, false)?;
        processed_guard += 1;
    }

    let Some(next_occurrence) = next_occurrence else {
        return Ok(ScheduleResult {
            updated_recurrence,
            next_scheduled: None,
            next_due: None,
        });
    };

    let next_scheduled = input
        .scheduled
        .as_deref()
        .map(|existing| format_like_existing(existing, next_occurrence))
        .or_else(|| Some(format_date_utc(next_occurrence)));
    let next_due = compute_next_due(input, next_occurrence);

    Ok(ScheduleResult {
        updated_recurrence,
        next_scheduled,
        next_due,
    })
}

fn compute_next_due(input: &RecurrenceInput, next_scheduled_date: DateTime<Utc>) -> Option<String> {
    let original_due = parse_date_string(input.due.as_deref()?)?;
    let original_scheduled = parse_date_string(input.scheduled.as_deref()?)?;
    let offset = original_due.signed_duration_since(original_scheduled);
    let next_due = next_scheduled_date + offset;
    Some(format_like_existing(input.due.as_deref()?, next_due))
}

fn get_next_occurrence_date(
    recurrence: &str,
    source_date: &str,
    after_date: DateTime<Utc>,
    inclusive: bool,
) -> anyhow::Result<Option<DateTime<Utc>>> {
    let set = parse_rrule_set(recurrence, source_date)?;
    for dt in set.into_iter().take(2000) {
        let utc = occurrence_to_utc(&dt);
        if utc > after_date || (inclusive && utc == after_date) {
            return Ok(Some(utc));
        }
    }
    Ok(None)
}

/// Convert an rrule occurrence to the UTC representation used for scheduling.
///
/// A date-only or floating DTSTART (no `Z`, no TZID) makes rrule generate
/// occurrences in `Tz::Local`. TaskNotes treats those values as calendar
/// dates/wall times stored at UTC midnight, so keep the wall time instead of
/// converting the instant: converting would move dates back a day in
/// timezones ahead of UTC (issue #8). Occurrences with a real timezone (`Z`
/// or TZID) are proper instants and are converted as before.
fn occurrence_to_utc(dt: &DateTime<rrule::Tz>) -> DateTime<Utc> {
    if dt.timezone().is_local() {
        DateTime::<Utc>::from_naive_utc_and_offset(dt.naive_local(), Utc)
    } else {
        dt.with_timezone(&Utc)
    }
}

fn parse_rrule_set(rule: &str, source_date: &str) -> anyhow::Result<RRuleSet> {
    let normalized = if let Some(dtstart_index) = rule.find("DTSTART:") {
        let after = &rule[dtstart_index + "DTSTART:".len()..];
        let semi = after.find(';');
        let (dtstart_value, remainder) = match semi {
            Some(idx) => (&after[..idx], &after[idx + 1..]),
            None => (after, ""),
        };
        if remainder.trim().is_empty() {
            format!("DTSTART:{dtstart_value}")
        } else if remainder.trim_start().starts_with("RRULE:") {
            format!("DTSTART:{dtstart_value}\n{}", remainder.trim())
        } else {
            format!("DTSTART:{dtstart_value}\nRRULE:{}", remainder.trim())
        }
    } else {
        let dtstart = format_dtstart_value(source_date)
            .ok_or_else(|| anyhow::anyhow!("invalid recurrence source date"))?;
        format!("DTSTART:{dtstart}\nRRULE:{rule}")
    };
    normalized
        .parse::<RRuleSet>()
        .map_err(|error| anyhow::anyhow!("invalid recurrence: {error}"))
}

fn add_dtstart_to_recurrence_rule(recurrence: &str, source_date: &str) -> Option<String> {
    if recurrence.is_empty() || recurrence.contains("DTSTART:") {
        return Some(recurrence.to_string());
    }
    let dtstart = format_dtstart_value(source_date)?;
    Some(format!("DTSTART:{dtstart};{recurrence}"))
}

fn update_dtstart_in_recurrence_rule(recurrence: &str, date_str: &str) -> Option<String> {
    if recurrence.is_empty() {
        return None;
    }
    let dtstart = format_dtstart_value(date_str)?;
    if let Some(start) = recurrence.find("DTSTART:") {
        let tail = &recurrence[start..];
        let end_offset = tail.find(';').unwrap_or(tail.len());
        let end = start + end_offset;
        let suffix = if end < recurrence.len() {
            &recurrence[end..]
        } else {
            ""
        };
        Some(format!("DTSTART:{dtstart}{suffix}"))
    } else {
        Some(format!("DTSTART:{dtstart};{recurrence}"))
    }
}

fn format_dtstart_value(date_str: &str) -> Option<String> {
    if date_str.contains('T') {
        let parsed = parse_date_string(date_str)?;
        Some(format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
            parsed.year(),
            parsed.month(),
            parsed.day(),
            parsed.hour(),
            parsed.minute(),
            parsed.second()
        ))
    } else {
        let parsed = parse_date_string(date_str)?;
        Some(format!(
            "{:04}{:02}{:02}",
            parsed.year(),
            parsed.month(),
            parsed.day()
        ))
    }
}

fn parse_date_string(date_str: &str) -> Option<DateTime<Utc>> {
    if date_str.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(date_str) {
        return Some(dt.with_timezone(&Utc));
    }
    let part = get_date_part(date_str);
    let date = NaiveDate::parse_from_str(&part, "%Y-%m-%d").ok()?;
    Some(DateTime::<Utc>::from_naive_utc_and_offset(
        date.and_hms_opt(0, 0, 0)?,
        Utc,
    ))
}

fn format_date_utc(date: DateTime<Utc>) -> String {
    date.format("%Y-%m-%d").to_string()
}

fn format_like_existing(existing: &str, value: DateTime<Utc>) -> String {
    if existing.contains('T') {
        value.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    } else {
        format_date_utc(value)
    }
}

pub fn uncomplete_instance(instances: &[String], target: &str) -> Value {
    let target = get_date_part(target);
    let next: Vec<String> = instances
        .iter()
        .filter(|value| get_date_part(value) != target)
        .cloned()
        .collect();
    json!({ "completeInstances": next })
}

pub fn skip_instance(instances: &[String], target: &str) -> Value {
    let target = get_date_part(target);
    let mut next = instances.to_vec();
    if !next.iter().any(|value| get_date_part(value) == target) {
        next.push(target);
    }
    json!({ "skippedInstances": next })
}

pub fn unskip_instance(instances: &[String], target: &str) -> Value {
    let target = get_date_part(target);
    let next: Vec<String> = instances
        .iter()
        .filter(|value| get_date_part(value) != target)
        .cloned()
        .collect();
    json!({ "skippedInstances": next })
}

pub fn effective_state(completed: &[String], skipped: &[String], target: &str) -> Value {
    let target = get_date_part(target);
    let completed = completed.iter().any(|value| get_date_part(value) == target);
    let skipped = skipped.iter().any(|value| get_date_part(value) == target);
    let state = if completed {
        "completed"
    } else if skipped {
        "skipped"
    } else {
        "open"
    };
    json!({ "value": state })
}

use chrono::Timelike;

#[cfg(test)]
mod tests {
    use super::*;

    fn first_occurrence(rule: &str) -> DateTime<Utc> {
        let set: RRuleSet = rule.parse().expect("valid rule");
        occurrence_to_utc(&set.into_iter().next().expect("an occurrence"))
    }

    #[test]
    fn utc_dtstart_occurrences_are_unchanged() {
        assert_eq!(
            first_occurrence("DTSTART:20261108T020000Z\nRRULE:FREQ=WEEKLY;BYDAY=SU").to_rfc3339(),
            "2026-11-08T02:00:00+00:00"
        );
    }

    #[test]
    fn tzid_dtstart_occurrences_keep_their_instant() {
        // 09:00 in New York is 14:00Z. Keeping the wall time here (09:00Z)
        // would shift the instant, so only Tz::Local keeps its wall time.
        assert_eq!(
            first_occurrence(
                "DTSTART;TZID=America/New_York:20261108T090000\nRRULE:FREQ=WEEKLY;BYDAY=SU"
            )
            .to_rfc3339(),
            "2026-11-08T14:00:00+00:00"
        );
    }
}
