use chrono::{DateTime, Datelike, TimeZone, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Schedule {
    Once { at: String },
    Interval { seconds: u64, start_at: String },
    Cron { expression: String },
}

pub fn timestamp(s: &str) -> Result<u64, String> {
    let t = DateTime::parse_from_rfc3339(s)
        .map_err(|_| "Use an RFC3339 date with an explicit UTC offset")?
        .timestamp();
    if !(0..=253402300799).contains(&t) {
        return Err("Date outside supported range".into());
    }
    Ok(t as u64)
}
pub fn iso(t: u64) -> String {
    Utc.timestamp_opt(t as i64, 0)
        .single()
        .map(|d| d.to_rfc3339())
        .unwrap_or_default()
}
impl Schedule {
    pub fn validate(&self, now: u64) -> Result<u64, String> {
        if let Self::Interval { seconds, .. } = self {
            if !(60..=31536000).contains(seconds) {
                return Err("Interval must be 60 seconds to 365 days".into());
            }
        }
        self.next_after(now).ok_or_else(|| {
            "Schedule has no future occurrence in the next eight years (cron uses UTC)".into()
        })
    }
    pub fn next_after(&self, now: u64) -> Option<u64> {
        match self {
            Self::Once { at } => timestamp(at).ok().filter(|t| *t > now),
            Self::Interval { seconds, start_at } => {
                if *seconds < 60 || *seconds > 31536000 {
                    return None;
                }
                let start = timestamp(start_at).ok()?;
                if start > now {
                    Some(start)
                } else {
                    start.checked_add(((now - start) / seconds + 1).checked_mul(*seconds)?)
                }
            }
            Self::Cron { expression } => Cron::parse(expression).ok()?.next(now),
        }
    }
}
#[derive(Debug)]
struct Cron {
    fields: Vec<Vec<u32>>,
    day_any: bool,
    week_any: bool,
}
impl Cron {
    fn parse(s: &str) -> Result<Self, String> {
        let p: Vec<_> = s.split_whitespace().collect();
        if p.len() != 5 || s.len() > 128 {
            return Err(
                "Cron requires five numeric UTC fields: minute hour day month weekday".into(),
            );
        }
        let mut fields = Vec::new();
        for (i, (lo, hi)) in [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)]
            .into_iter()
            .enumerate()
        {
            let mut out = Vec::new();
            for part in p[i].split(',') {
                let (range, step) = if let Some((r, st)) = part.split_once('/') {
                    (r, st.parse::<u32>().map_err(|_| "Invalid cron step")?)
                } else {
                    (part, 1)
                };
                if step == 0 || step > hi - lo + 1 {
                    return Err("Invalid cron step".into());
                }
                let (a, b) = if range == "*" {
                    (lo, hi)
                } else if let Some((a, b)) = range.split_once('-') {
                    (
                        a.parse().map_err(|_| "Invalid cron range")?,
                        b.parse().map_err(|_| "Invalid cron range")?,
                    )
                } else {
                    let n = range.parse().map_err(|_| "Invalid cron number")?;
                    (n, if part.contains('/') { hi } else { n })
                };
                if a < lo || b > hi || a > b {
                    return Err("Cron value outside range".into());
                }
                out.extend((a..=b).step_by(step as usize).map(|v| {
                    if i == 4 && v == 7 {
                        0
                    } else {
                        v
                    }
                }));
            }
            out.sort_unstable();
            out.dedup();
            if out.is_empty() {
                return Err("Empty cron field".into());
            }
            fields.push(out);
        }
        Ok(Self {
            fields,
            day_any: p[2].starts_with('*'),
            week_any: p[4].starts_with('*'),
        })
    }
    fn next(&self, now: u64) -> Option<u64> {
        let d = Utc.timestamp_opt(i64::try_from(now).ok()?, 0).single()?;
        let mut date = d.date_naive();
        for _ in 0..=366 * 8 {
            let dm = self.fields[2].contains(&date.day());
            let dw = self.fields[4].contains(&date.weekday().num_days_from_sunday());
            // Vixie cron: restricted day-of-month and weekday use OR; a wildcard uses AND.
            let day = if !self.day_any && !self.week_any {
                dm || dw
            } else {
                dm && dw
            };
            if self.fields[3].contains(&date.month()) && day {
                for h in &self.fields[1] {
                    for m in &self.fields[0] {
                        let t = date.and_hms_opt(*h, *m, 0)?.and_utc().timestamp();
                        if t >= 0 && t as u64 > now {
                            return Some(t as u64);
                        }
                    }
                }
            }
            date = date.succ_opt()?;
        }
        None
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn t(s: &str) -> u64 {
        timestamp(s).unwrap()
    }
    #[test]
    fn cron_calendar() {
        let c = Schedule::Cron {
            expression: "*/15 9-17 * * 1-5".into(),
        };
        assert_eq!(
            c.next_after(t("2026-09-30T09:01:00Z")),
            Some(t("2026-09-30T09:15:00Z"))
        );
        assert_eq!(
            c.next_after(t("2026-10-02T17:59:00Z")),
            Some(t("2026-10-05T09:00:00Z"))
        );
    }
    #[test]
    fn leap_and_impossible() {
        assert_eq!(
            Schedule::Cron {
                expression: "0 0 29 2 *".into()
            }
            .next_after(t("2026-01-01T00:00:00Z")),
            Some(t("2028-02-29T00:00:00Z"))
        );
        assert!(Schedule::Cron {
            expression: "0 0 31 2 *".into()
        }
        .next_after(t("2026-01-01T00:00:00Z"))
        .is_none());
    }
    #[test]
    fn day_or_and_utc() {
        let c = Cron::parse("0 0 1 * 1").unwrap();
        assert_eq!(
            c.next(t("2026-09-30T00:00:00Z")),
            Some(t("2026-10-01T00:00:00Z"))
        );
        assert_eq!(t("2026-09-30T09:00:00-07:00"), t("2026-09-30T16:00:00Z"));
    }
    #[test]
    fn reject_bad() {
        for s in [
            "* * * *",
            "* * * * * *",
            "*/0 * * * *",
            "0 24 * * *",
            "0 0 * 13 *",
            "0 0 * * -1",
        ] {
            assert!(Cron::parse(s).is_err(), "{s}");
        }
    }
    #[test]
    fn interval_skips_without_drift() {
        let c = Schedule::Interval {
            seconds: 60,
            start_at: "2026-09-30T00:00:00Z".into(),
        };
        assert_eq!(
            c.next_after(t("2026-09-30T03:00:33Z")),
            Some(t("2026-09-30T03:01:00Z"))
        );
    }
}
