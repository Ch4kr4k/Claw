//! Session-scoped scheduled prompts, like Claude Code's: cron tasks
//! (`CronCreate`, recurring or one-shot, in local time) and the single
//! pending wake-up of a self-paced `/loop` (`ScheduleWakeup`). Nothing runs
//! here: the REPL asks for due prompts while it is idle and runs them as
//! turns, so a scheduled prompt never interrupts a running one.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Datelike, Local, TimeZone, Timelike};

/// A parsed five-field cron expression: minute, hour, day of month, month,
/// day of week (0 or 7 is Sunday). Fields take `*`, numbers, `a-b` ranges,
/// `,` lists and `/n` steps; `@hourly`, `@daily`, `@weekly`, `@monthly` and
/// `@yearly` are accepted too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    minutes: Vec<bool>,
    hours: Vec<bool>,
    days: Vec<bool>,
    months: Vec<bool>,
    weekdays: Vec<bool>,
    /// Day of month and day of week were both restricted: either matches.
    either_day: bool,
}

impl CronSchedule {
    pub fn parse(expression: &str) -> Result<Self, String> {
        let expression = match expression.trim() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            other => other,
        };
        let fields: Vec<&str> = expression.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields.as_slice() else {
            return Err(format!(
                "`{expression}` is not a cron expression: it needs 5 fields (minute hour day month weekday), e.g. `*/5 * * * *`"
            ));
        };
        let mut weekdays = parse_field(weekday, 0, 7, "weekday")?;
        if weekdays[7] {
            weekdays[0] = true;
        }
        Ok(Self {
            minutes: parse_field(minute, 0, 59, "minute")?,
            hours: parse_field(hour, 0, 23, "hour")?,
            days: parse_field(day, 1, 31, "day of month")?,
            months: parse_field(month, 1, 12, "month")?,
            weekdays,
            either_day: *day != "*" && *weekday != "*",
        })
    }

    fn matches<Tz: TimeZone>(&self, time: &DateTime<Tz>) -> bool {
        let day = self.days[time.day() as usize];
        let weekday = self.weekdays[time.weekday().num_days_from_sunday() as usize];
        let day_ok = if self.either_day {
            day || weekday
        } else {
            day && weekday
        };
        self.minutes[time.minute() as usize]
            && self.hours[time.hour() as usize]
            && self.months[time.month() as usize]
            && day_ok
    }

    /// The first whole minute strictly after `after` that matches, within
    /// about five years.
    pub fn next_after<Tz: TimeZone>(&self, after: &DateTime<Tz>) -> Option<DateTime<Tz>> {
        let mut time = after.clone() + chrono::Duration::seconds(60 - i64::from(after.second()));
        time = time.with_nanosecond(0)?;
        for _ in 0..(366 * 24 * 60 * 5) {
            if !self.months[time.month() as usize] {
                // Skip to the start of the next month.
                let (year, month) = if time.month() == 12 {
                    (time.year() + 1, 1)
                } else {
                    (time.year(), time.month() + 1)
                };
                time = time
                    .timezone()
                    .with_ymd_and_hms(year, month, 1, 0, 0, 0)
                    .earliest()?;
                continue;
            }
            if self.matches(&time) {
                return Some(time);
            }
            time += chrono::Duration::minutes(1);
        }
        None
    }
}

fn parse_field(field: &str, low: u32, high: u32, name: &str) -> Result<Vec<bool>, String> {
    let mut allowed = vec![false; high as usize + 1];
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (
                range,
                step.parse::<u32>()
                    .ok()
                    .filter(|step| *step > 0)
                    .ok_or_else(|| format!("bad step `{step}` in the {name} field"))?,
            ),
            None => (part, 1),
        };
        let (start, end) = if range == "*" {
            (low, high)
        } else if let Some((a, b)) = range.split_once('-') {
            (number(a, low, high, name)?, number(b, low, high, name)?)
        } else {
            let start = number(range, low, high, name)?;
            // `5/15` means from 5 to the end, every 15.
            (start, if part.contains('/') { high } else { start })
        };
        if start > end {
            return Err(format!(
                "range `{range}` runs backwards in the {name} field"
            ));
        }
        let mut value = start;
        while value <= end {
            allowed[value as usize] = true;
            value += step;
        }
    }
    Ok(allowed)
}

fn number(text: &str, low: u32, high: u32, name: &str) -> Result<u32, String> {
    text.parse::<u32>()
        .ok()
        .filter(|value| (low..=high).contains(value))
        .ok_or_else(|| format!("`{text}` is not a valid {name} ({low}-{high})"))
}

/// A cron task: its prompt and when it next runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronTask {
    pub id: String,
    pub expression: String,
    pub prompt: String,
    pub recurring: bool,
    pub next_run: DateTime<Local>,
    pub runs: u64,
}

/// A prompt that is due now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuePrompt {
    /// The cron task's id, or `wakeup`.
    pub source: String,
    pub prompt: String,
}

#[derive(Debug)]
struct Wakeup {
    at: Instant,
    prompt: String,
    reason: Option<String>,
}

#[derive(Debug, Default)]
pub struct Scheduler {
    tasks: Mutex<BTreeMap<String, CronTask>>,
    wakeup: Mutex<Option<Wakeup>>,
    next_id: Mutex<u64>,
}

/// The session's scheduler, shared by the cron tools, `ScheduleWakeup` and
/// the REPL.
pub fn global_scheduler() -> &'static Scheduler {
    static SCHEDULER: OnceLock<Scheduler> = OnceLock::new();
    SCHEDULER.get_or_init(Scheduler::default)
}

/// Most cron tasks a session keeps.
const MAX_TASKS: usize = 50;

impl Scheduler {
    /// Adds a cron task; returns it with its first run time.
    pub fn add_cron(
        &self,
        expression: &str,
        prompt: &str,
        recurring: bool,
    ) -> Result<CronTask, String> {
        if prompt.trim().is_empty() {
            return Err("prompt must not be empty".to_string());
        }
        let schedule = CronSchedule::parse(expression)?;
        let next_run = schedule
            .next_after(&Local::now())
            .ok_or_else(|| format!("`{expression}` never matches a date"))?;
        let mut tasks = lock(&self.tasks);
        if tasks.len() >= MAX_TASKS {
            return Err(format!(
                "at most {MAX_TASKS} scheduled tasks; delete one first"
            ));
        }
        let id = {
            let mut next = lock(&self.next_id);
            *next += 1;
            format!("cron{next}")
        };
        let task = CronTask {
            id: id.clone(),
            expression: expression.trim().to_string(),
            prompt: prompt.to_string(),
            recurring,
            next_run,
            runs: 0,
        };
        tasks.insert(id, task.clone());
        Ok(task)
    }

    pub fn delete(&self, id: &str) -> Result<CronTask, String> {
        let mut tasks = lock(&self.tasks);
        tasks.remove(id).ok_or_else(|| {
            let ids: Vec<&String> = tasks.keys().collect();
            if ids.is_empty() {
                format!("no scheduled task `{id}`; none are scheduled")
            } else {
                format!(
                    "no scheduled task `{id}`; scheduled: {}",
                    ids.iter()
                        .map(|id| id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        })
    }

    #[must_use]
    pub fn list(&self) -> Vec<CronTask> {
        lock(&self.tasks).values().cloned().collect()
    }

    /// Sets the pending wake-up (replacing any other) `delay` from now.
    pub fn set_wakeup(&self, delay: Duration, prompt: &str, reason: Option<&str>) {
        *lock(&self.wakeup) = Some(Wakeup {
            at: Instant::now() + delay,
            prompt: prompt.to_string(),
            reason: reason.map(str::to_string),
        });
    }

    /// Cancels the pending wake-up; true when there was one.
    pub fn cancel_wakeup(&self) -> bool {
        lock(&self.wakeup).take().is_some()
    }

    /// Seconds until the pending wake-up and why it was set.
    #[must_use]
    pub fn wakeup(&self) -> Option<(u64, Option<String>)> {
        lock(&self.wakeup).as_ref().map(|wakeup| {
            (
                wakeup
                    .at
                    .saturating_duration_since(Instant::now())
                    .as_secs(),
                wakeup.reason.clone(),
            )
        })
    }

    /// Whether a prompt is due (cheap; polled while the REPL is idle).
    #[must_use]
    pub fn has_due(&self) -> bool {
        let now = Local::now();
        lock(&self.wakeup)
            .as_ref()
            .is_some_and(|wakeup| wakeup.at <= Instant::now())
            || lock(&self.tasks).values().any(|task| task.next_run <= now)
    }

    /// Takes the prompts that are due: one-shot tasks are removed and
    /// recurring ones moved to their next run. A task that missed several
    /// runs (the machine slept) runs once.
    pub fn take_due(&self) -> Vec<DuePrompt> {
        let now = Local::now();
        let mut due = Vec::new();
        {
            let mut tasks = lock(&self.tasks);
            let mut finished = Vec::new();
            for task in tasks.values_mut() {
                if task.next_run > now {
                    continue;
                }
                task.runs += 1;
                due.push(DuePrompt {
                    source: task.id.clone(),
                    prompt: task.prompt.clone(),
                });
                let next = CronSchedule::parse(&task.expression)
                    .ok()
                    .and_then(|schedule| schedule.next_after(&now));
                match next {
                    Some(next) if task.recurring => task.next_run = next,
                    _ => finished.push(task.id.clone()),
                }
            }
            for id in finished {
                tasks.remove(&id);
            }
        }
        let mut wakeup = lock(&self.wakeup);
        if wakeup.as_ref().is_some_and(|w| w.at <= Instant::now()) {
            if let Some(w) = wakeup.take() {
                due.push(DuePrompt {
                    source: "wakeup".to_string(),
                    prompt: w.prompt,
                });
            }
        }
        due
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{CronSchedule, Scheduler};
    use chrono::{TimeZone, Utc};
    use std::time::Duration;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn steps_ranges_and_lists() {
        let every_five = CronSchedule::parse("*/5 * * * *").unwrap();
        assert_eq!(
            every_five.next_after(&at(2026, 1, 1, 10, 3)),
            Some(at(2026, 1, 1, 10, 5))
        );
        let weekday_nine = CronSchedule::parse("0 9 * * 1-5").unwrap();
        // 2026-01-03 is a Saturday: next is Monday the 5th.
        assert_eq!(
            weekday_nine.next_after(&at(2026, 1, 3, 12, 0)),
            Some(at(2026, 1, 5, 9, 0))
        );
        let list = CronSchedule::parse("15,45 8 * * *").unwrap();
        assert_eq!(
            list.next_after(&at(2026, 1, 1, 8, 15)),
            Some(at(2026, 1, 1, 8, 45))
        );
        let monthly = CronSchedule::parse("@monthly").unwrap();
        assert_eq!(
            monthly.next_after(&at(2026, 1, 15, 0, 0)),
            Some(at(2026, 2, 1, 0, 0))
        );
        let leap = CronSchedule::parse("0 0 29 2 *").unwrap();
        assert_eq!(
            leap.next_after(&at(2026, 3, 1, 0, 0)),
            Some(at(2028, 2, 29, 0, 0))
        );
    }

    #[test]
    fn rejects_bad_expressions() {
        assert!(CronSchedule::parse("* * *").is_err());
        assert!(CronSchedule::parse("61 * * * *").is_err());
        assert!(CronSchedule::parse("*/0 * * * *").is_err());
        assert!(CronSchedule::parse("5-2 * * * *").is_err());
    }

    #[test]
    fn wakeups_come_due_once() {
        let scheduler = Scheduler::default();
        scheduler.set_wakeup(Duration::from_millis(0), "check again", Some("polling"));
        assert!(scheduler.has_due());
        let due = scheduler.take_due();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].prompt, "check again");
        assert!(!scheduler.has_due());
        assert!(scheduler.take_due().is_empty());
    }

    #[test]
    fn cron_tasks_list_and_delete() {
        let scheduler = Scheduler::default();
        let task = scheduler.add_cron("*/10 * * * *", "status", true).unwrap();
        assert_eq!(scheduler.list().len(), 1);
        assert!(!scheduler.has_due(), "first run is in the future");
        assert!(scheduler.delete(&task.id).is_ok());
        assert!(scheduler.delete(&task.id).is_err());
        assert!(scheduler.add_cron("bad", "x", true).is_err());
    }
}
