use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDateTime, Offset, TimeZone, Utc};
use chrono_tz::Tz;

const SEARCH_DAYS: usize = 366 * 8;
const MAX_OFFSET_GAP_MINUTES: i64 = 24 * 60;
const MONTH_NAMES: [&str; 12] = [
    "JANUARY",
    "FEBRUARY",
    "MARCH",
    "APRIL",
    "MAY",
    "JUNE",
    "JULY",
    "AUGUST",
    "SEPTEMBER",
    "OCTOBER",
    "NOVEMBER",
    "DECEMBER",
];
const WEEKDAY_NAMES: [&str; 7] = [
    "SUNDAY",
    "MONDAY",
    "TUESDAY",
    "WEDNESDAY",
    "THURSDAY",
    "FRIDAY",
    "SATURDAY",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidCronSchedule {
    #[error("the cron schedule must be a valid five-field expression or supported alias")]
    Expression,
    #[error("the cron schedule has no occurrence within the next eight years")]
    NoOccurrence,
}

#[derive(Debug, Clone)]
struct Field {
    values: Vec<u32>,
    wildcard: bool,
}

impl Field {
    fn parse(text: &str, minimum: u32, maximum: u32, names: &[&str]) -> Result<Self, InvalidCronSchedule> {
        let mut values = Vec::new();
        for part in text.split(',') {
            let (range, step) = match part.split_once('/') {
                Some((range, step)) => (
                    range,
                    step.parse::<i8>().map_err(|_| InvalidCronSchedule::Expression)?,
                ),
                None => (part, 1),
            };
            if step <= 0 {
                return Err(InvalidCronSchedule::Expression);
            }
            let number = |value: &str| -> Result<u32, InvalidCronSchedule> {
                let value = names
                    .iter()
                    .position(|name| {
                        value.eq_ignore_ascii_case(name) || value.eq_ignore_ascii_case(&name[..3])
                    })
                    .map_or_else(
                        || {
                            value
                                .parse::<i8>()
                                .map(|number| number as u32)
                                .map_err(|_| InvalidCronSchedule::Expression)
                        },
                        |index| Ok(index as u32 + minimum),
                    )?;
                if !(minimum..=maximum).contains(&value) {
                    return Err(InvalidCronSchedule::Expression);
                }
                Ok(value)
            };
            let (start, end) = if range == "*" {
                (minimum, maximum)
            } else if range.starts_with('-') && !range[1..].contains('-') {
                let start = number(range)?;
                (start, if part.contains('/') { maximum } else { start })
            } else if let Some((start, end)) = range.split_once('-') {
                (number(start)?, number(end)?)
            } else {
                let start = number(range)?;
                (start, if part.contains('/') { maximum } else { start })
            };
            if start > end {
                return Err(InvalidCronSchedule::Expression);
            }
            values.extend((start..=end).step_by(step as usize));
        }
        values.sort_unstable();
        values.dedup();
        Ok(Self {
            values,
            wildcard: text == "*",
        })
    }

    fn contains(&self, value: u32) -> bool {
        self.values.binary_search(&value).is_ok()
    }
}

#[derive(Debug, Clone)]
pub struct ParsedSchedule {
    minute: Field,
    hour: Field,
    day: Field,
    month: Field,
    weekday: Field,
}

impl ParsedSchedule {
    pub fn parse(expression: &str) -> Result<Self, InvalidCronSchedule> {
        if expression
            .chars()
            .any(|character| character.is_whitespace() && !matches!(character, ' ' | '\t'))
        {
            return Err(InvalidCronSchedule::Expression);
        }
        let normalized = expression.trim_matches([' ', '\t']).to_ascii_lowercase();
        let expression = match normalized.as_str() {
            "@yearly" | "@annually" => "0 0 1 1 *",
            "@monthly" => "0 0 1 * *",
            "@weekly" => "0 0 * * 0",
            "@daily" | "@midnight" => "0 0 * * *",
            "@hourly" => "0 * * * *",
            expression => expression,
        };
        let fields: Vec<_> = expression.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields.as_slice() else {
            return Err(InvalidCronSchedule::Expression);
        };
        let mut weekday = Field::parse(weekday, 0, 7, &WEEKDAY_NAMES)?;
        for value in &mut weekday.values {
            if *value == 7 {
                *value = 0;
            }
        }
        weekday.values.sort_unstable();
        weekday.values.dedup();
        Ok(Self {
            minute: Field::parse(minute, 0, 59, &[])?,
            hour: Field::parse(hour, 0, 23, &[])?,
            day: Field::parse(day, 1, 31, &[])?,
            month: Field::parse(month, 1, 12, &MONTH_NAMES)?,
            weekday,
        })
    }

    pub fn next_after(
        &self,
        after: DateTime<Utc>,
        time_zone: Tz,
    ) -> Result<DateTime<Utc>, InvalidCronSchedule> {
        let mut date = after.with_timezone(&time_zone).date_naive();
        for _ in 0..SEARCH_DAYS {
            let day_matches = self.day.contains(date.day());
            let weekday_matches = self.weekday.contains(date.weekday().num_days_from_sunday());
            let matches = if self.day.wildcard || self.weekday.wildcard {
                day_matches && weekday_matches
            } else {
                day_matches || weekday_matches
            };
            if self.month.contains(date.month()) && matches {
                let mut next = None;
                for hour in &self.hour.values {
                    for minute in &self.minute.values {
                        let local = date
                            .and_hms_opt(*hour, *minute, 0)
                            .ok_or(InvalidCronSchedule::Expression)?;
                        if let Some(candidate) = local_occurrence(local, time_zone) {
                            if candidate > after && next.is_none_or(|held| candidate < held) {
                                next = Some(candidate);
                            }
                        }
                    }
                }
                if let Some(next) = next {
                    return Ok(next);
                }
            }
            date = date.succ_opt().ok_or(InvalidCronSchedule::NoOccurrence)?;
        }
        Err(InvalidCronSchedule::NoOccurrence)
    }
}

fn local_occurrence(local: NaiveDateTime, time_zone: Tz) -> Option<DateTime<Utc>> {
    match time_zone.from_local_datetime(&local) {
        LocalResult::Single(value) => Some(value.with_timezone(&Utc)),
        LocalResult::Ambiguous(first, second) => Some(first.min(second).with_timezone(&Utc)),
        LocalResult::None => {
            let mut before = None;
            let mut after = None;
            for minutes in 1..=MAX_OFFSET_GAP_MINUTES {
                let distance = Duration::minutes(minutes);
                if before.is_none() {
                    before = time_zone
                        .from_local_datetime(&local.checked_sub_signed(distance)?)
                        .earliest();
                }
                if after.is_none() {
                    after = time_zone
                        .from_local_datetime(&local.checked_add_signed(distance)?)
                        .earliest();
                }
                if let (Some(before), Some(after)) = (&before, &after) {
                    let gap =
                        after.offset().fix().local_minus_utc() - before.offset().fix().local_minus_utc();
                    let shifted = local.checked_add_signed(Duration::seconds(i64::from(gap)))?;
                    return time_zone
                        .from_local_datetime(&shifted)
                        .earliest()
                        .map(|value| value.with_timezone(&Utc));
                }
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(value: &str) -> DateTime<Utc> {
        value.parse().unwrap()
    }
    fn next(expression: &str, after: &str, zone: Tz) -> DateTime<Utc> {
        ParsedSchedule::parse(expression)
            .unwrap()
            .next_after(at(after), zone)
            .unwrap()
    }

    #[test]
    fn the_next_run_is_strictly_after_the_given_instant() {
        assert_eq!(
            next("* * * * *", "2026-01-01T00:00:00Z", chrono_tz::UTC),
            at("2026-01-01T00:01:00Z")
        );
        assert_eq!(
            next("@hourly", "2026-01-01T00:00:30Z", chrono_tz::UTC),
            at("2026-01-01T01:00:00Z")
        );
    }

    #[test]
    fn names_lists_ranges_steps_and_sunday_seven_are_understood() {
        assert_eq!(
            next(
                "5,20-40/10 2 * JAN-MAR/2 7",
                "2026-01-01T00:00:00Z",
                chrono_tz::UTC
            ),
            at("2026-01-04T02:05:00Z")
        );
    }

    #[test]
    fn full_month_and_weekday_names_match_the_nibrun_schedule_contract() {
        for (expression, expected) in [
            ("0 9 * * Monday-Friday", "2026-10-01T09:00:00Z"),
            ("0 0 1 january *", "2027-01-01T00:00:00Z"),
            ("0 0 29 2 *", "2028-02-29T00:00:00Z"),
            ("*/15 * * * *", "2026-09-30T09:45:00Z"),
        ] {
            assert_eq!(
                next(expression, "2026-09-30T09:41:56Z", chrono_tz::UTC),
                at(expected)
            );
        }
    }

    #[test]
    fn day_and_weekday_restrictions_match_either_day() {
        assert_eq!(
            next("0 0 15 * MON", "2026-01-01T00:00:00Z", chrono_tz::UTC),
            at("2026-01-05T00:00:00Z")
        );
    }

    #[test]
    fn a_nonexistent_spring_hour_moves_forward_by_the_offset_gap() {
        assert_eq!(
            next("30 2 * * *", "2026-03-28T02:00:00Z", chrono_tz::Europe::Zurich),
            at("2026-03-29T01:30:00Z")
        );
    }

    #[test]
    fn an_ambiguous_autumn_hour_runs_only_at_its_first_occurrence() {
        let first = next("30 2 * * *", "2026-10-24T23:00:00Z", chrono_tz::Europe::Zurich);
        assert_eq!(first, at("2026-10-25T00:30:00Z"));
        assert_eq!(
            next("30 2 * * *", "2026-10-25T00:30:00Z", chrono_tz::Europe::Zurich),
            at("2026-10-26T01:30:00Z")
        );
    }

    #[test]
    fn malformed_and_reboot_schedules_are_rejected() {
        for expression in [
            "@reboot",
            "* * * *",
            "60 * * * *",
            "*/0 * * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 8",
            "9-3 * * * *",
        ] {
            assert!(ParsedSchedule::parse(expression).is_err(), "{expression}");
        }
        assert_eq!(
            ParsedSchedule::parse("0 0 31 FEB *")
                .unwrap()
                .next_after(at("2026-01-01T00:00:00Z"), chrono_tz::UTC),
            Err(InvalidCronSchedule::NoOccurrence)
        );
    }

    #[test]
    fn schedule_grammar_and_day_matching_follow_bun_1_4_2_fixtures() {
        for (expression, expected) in [
            ("@DAILY", "2026-01-02T00:00:00Z"),
            (" @Daily ", "2026-01-02T00:00:00Z"),
            ("*/100 * * * *", "2026-01-01T01:00:00Z"),
            ("*/127 * * * *", "2026-01-01T01:00:00Z"),
            ("0 0 */2 * MON", "2026-01-03T00:00:00Z"),
            ("0 0 *,1 * MON", "2026-01-02T00:00:00Z"),
            ("0 0 15 * */1", "2026-01-02T00:00:00Z"),
            ("0 0 15 * *", "2026-01-15T00:00:00Z"),
            ("0 0 * * MON/2", "2026-01-02T00:00:00Z"),
            ("0 0 * * -0", "2026-01-04T00:00:00Z"),
            ("+1-+3 * * * *", "2026-01-01T00:01:00Z"),
        ] {
            assert_eq!(
                next(expression, "2026-01-01T00:00:00Z", chrono_tz::UTC),
                at(expected),
                "{expression}"
            );
        }
        for expression in [
            "*/128 * * * *",
            "*/-0 * * * *",
            "0 0 * * FRI-MON",
            "0 0 * DEC-FEB *",
            "-0-2 * * * *",
            "0 0 * * ?",
            "0 0 * * *\r",
            "0 0 * * *\n",
        ] {
            assert!(ParsedSchedule::parse(expression).is_err(), "{expression}");
        }
    }
}
