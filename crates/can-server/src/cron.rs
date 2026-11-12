//! Cron-Ausdrücke mit fünf Feldern (Minute, Stunde, Tag, Monat, Wochentag), immer in UTC.
//!
//! Erlaubt sind `*`, Zahlen, Bereiche `a-b`, Listen `a,b` und Schritte `*/n` oder `a-b/n`,
//! außerdem `@hourly`, `@daily`, `@weekly`, `@monthly` und `@yearly`. Wie bei klassischem cron
//! genügt es, wenn Tag oder Wochentag passt, sobald beide eingeschränkt sind.

use std::fmt;
use std::str::FromStr;

/// Minuten pro Tag.
const DAY: u64 = 24 * 60;
/// So weit sucht [`Cron::next_after`] höchstens: fünf Jahre, genug für den 29. Februar.
const SEARCH_DAYS: u64 = 5 * 366;

/// Ein geprüfter Cron-Ausdruck.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cron {
    source: String,
    minutes: u64,
    hours: u64,
    days: u64,
    months: u64,
    weekdays: u64,
    /// Tag oder Wochentag war `*`; dann muss nur das andere passen.
    any_day: bool,
    any_weekday: bool,
}

/// Fehler beim Lesen eines Ausdrucks.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("ungültiger Cron-Ausdruck {expression:?}: {problem}")]
pub struct CronError {
    pub expression: String,
    pub problem: String,
}

/// Liest ein Feld als Bitmaske über `min..=max`.
fn field(text: &str, min: u64, max: u64) -> Result<u64, String> {
    let mut mask = 0u64;
    for item in text.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => {
                let step: u64 = step.parse().map_err(|_| format!("Schritt {step:?}"))?;
                if step == 0 {
                    return Err("Schritt 0".to_owned());
                }
                (range, step)
            }
            None => (item, 1),
        };
        let number = |value: &str| -> Result<u64, String> {
            let number: u64 = value.parse().map_err(|_| format!("Wert {value:?}"))?;
            if (min..=max).contains(&number) {
                Ok(number)
            } else {
                Err(format!("{number} liegt nicht in {min}-{max}"))
            }
        };
        let (from, to) = match range {
            "*" => (min, max),
            _ => match range.split_once('-') {
                Some((from, to)) => (number(from)?, number(to)?),
                // `5/15` heißt: ab 5 jede 15.
                None if step > 1 => (number(range)?, max),
                None => {
                    let value = number(range)?;
                    (value, value)
                }
            },
        };
        if from > to {
            return Err(format!("Bereich {range:?} ist leer"));
        }
        for value in (from..=to).step_by(usize::try_from(step).unwrap_or(usize::MAX)) {
            mask |= 1 << value;
        }
    }
    Ok(mask)
}

impl FromStr for Cron {
    type Err = CronError;

    fn from_str(expression: &str) -> Result<Self, CronError> {
        let failed = |problem: String| CronError {
            expression: expression.to_owned(),
            problem,
        };
        let expanded = match expression.trim() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            other => other,
        };
        let parts: Vec<&str> = expanded.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = parts.as_slice() else {
            return Err(failed(format!("{} statt 5 Felder", parts.len())));
        };
        let mut weekdays = field(weekday, 0, 7).map_err(failed)?;
        // 7 ist wie 0 der Sonntag.
        if weekdays & (1 << 7) != 0 {
            weekdays = (weekdays | 1) & !(1 << 7);
        }
        Ok(Self {
            source: expression.trim().to_owned(),
            minutes: field(minute, 0, 59).map_err(failed)?,
            hours: field(hour, 0, 23).map_err(failed)?,
            days: field(day, 1, 31).map_err(failed)?,
            months: field(month, 1, 12).map_err(failed)?,
            weekdays,
            any_day: day.starts_with('*'),
            any_weekday: weekday.starts_with('*'),
        })
    }
}

impl<'de> serde::Deserialize<'de> for Cron {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for Cron {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

/// Kalenderdatum zu Tagen seit 1970-01-01 (Algorithmus von Howard Hinnant).
pub fn civil(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

impl Cron {
    fn day_matches(&self, days: u64) -> bool {
        let (_, month, day) = civil(days);
        // Der 1.1.1970 war ein Donnerstag.
        let weekday = (days + 4) % 7;
        let by_day = self.days & (1 << day) != 0;
        let by_weekday = self.weekdays & (1 << weekday) != 0;
        let day_ok = match (self.any_day, self.any_weekday) {
            (true, true) => true,
            (false, true) => by_day,
            (true, false) => by_weekday,
            (false, false) => by_day || by_weekday,
        };
        self.months & (1 << month) != 0 && day_ok
    }

    /// Passt der Ausdruck auf diese Minute (Minuten seit 1970-01-01 00:00 UTC)?
    pub fn matches(&self, minute: u64) -> bool {
        let (days, rest) = (minute / DAY, minute % DAY);
        self.hours & (1 << (rest / 60)) != 0
            && self.minutes & (1 << (rest % 60)) != 0
            && self.day_matches(days)
    }

    /// Die erste passende Minute nach `minute`; `None`, wenn es in fünf Jahren keine gibt.
    pub fn next_after(&self, minute: u64) -> Option<u64> {
        let start = minute + 1;
        let first_day = start / DAY;
        (first_day..first_day + SEARCH_DAYS)
            .filter(|days| self.day_matches(*days))
            .find_map(|days| {
                let from = if days == first_day { start % DAY } else { 0 };
                (from..DAY)
                    .find(|rest| {
                        self.hours & (1 << (rest / 60)) != 0
                            && self.minutes & (1 << (rest % 60)) != 0
                    })
                    .map(|rest| days * DAY + rest)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minute seit 1970 für ein Datum in UTC.
    fn at(year: u64, month: u64, day: u64, hour: u64, minute: u64) -> u64 {
        let days = (0..30_000)
            .find(|d| civil(*d) == (year, month, day))
            .unwrap();
        days * DAY + hour * 60 + minute
    }

    #[test]
    fn civil_dates_are_correct() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(11_016), (2000, 2, 29));
        assert_eq!(civil(20_769), (2026, 11, 12));
    }

    #[test]
    fn fields_ranges_lists_and_steps() {
        let cron: Cron = "*/15 9-17 * * 1-5".parse().unwrap();
        // 12.11.2026 ist ein Donnerstag.
        assert!(cron.matches(at(2026, 11, 12, 9, 45)));
        assert!(!cron.matches(at(2026, 11, 12, 9, 50)));
        assert!(!cron.matches(at(2026, 11, 12, 18, 0)));
        assert!(!cron.matches(at(2026, 11, 14, 10, 0)), "Samstag");
        let listed: Cron = "5,10 0 1 1,7 *".parse().unwrap();
        assert!(listed.matches(at(2027, 7, 1, 0, 10)));
        assert!(!listed.matches(at(2027, 8, 1, 0, 10)));
        let from: Cron = "5/20 * * * *".parse().unwrap();
        assert!(from.matches(at(2026, 1, 1, 3, 45)));
        assert!(!from.matches(at(2026, 1, 1, 3, 0)));
    }

    #[test]
    fn day_or_weekday_and_sunday_as_seven() {
        // Am 13. oder an jedem Freitag.
        let cron: Cron = "0 12 13 * 5".parse().unwrap();
        assert!(cron.matches(at(2026, 11, 13, 12, 0)));
        assert!(cron.matches(at(2026, 11, 20, 12, 0)), "Freitag");
        assert!(!cron.matches(at(2026, 11, 19, 12, 0)));
        let sunday: Cron = "0 0 * * 7".parse().unwrap();
        assert!(sunday.matches(at(2026, 11, 15, 0, 0)));
        assert_eq!(
            sunday,
            "@weekly"
                .parse::<Cron>()
                .map(|c| Cron {
                    source: "0 0 * * 7".into(),
                    ..c
                })
                .unwrap()
        );
    }

    #[test]
    fn next_after_finds_the_next_minute() {
        let daily: Cron = "@daily".parse().unwrap();
        let now = at(2026, 11, 12, 8, 30);
        assert_eq!(daily.next_after(now), Some(at(2026, 11, 13, 0, 0)));
        let leap: Cron = "0 6 29 2 *".parse().unwrap();
        assert_eq!(leap.next_after(now), Some(at(2028, 2, 29, 6, 0)));
        let soon: Cron = "*/10 * * * *".parse().unwrap();
        assert_eq!(soon.next_after(now), Some(at(2026, 11, 12, 8, 40)));
        let never: Cron = "0 0 31 2 *".parse().unwrap();
        assert_eq!(never.next_after(now), None);
        assert_eq!(daily.to_string(), "@daily");
    }

    #[test]
    fn bad_expressions_are_rejected() {
        for bad in [
            "",
            "* * * *",
            "60 * * * *",
            "* * 0 * *",
            "*/0 * * * *",
            "5-1 * * * *",
            "a * * * *",
        ] {
            assert!(bad.parse::<Cron>().is_err(), "{bad:?}");
        }
        let error = "* * *".parse::<Cron>().unwrap_err();
        assert!(error.to_string().contains("3 statt 5"));
    }
}
