//! "Last Saturday", "the weekend", "October 31st": the days people mean.
//!
//! Small models are unreliable at calendar arithmetic, so dates are resolved
//! here, in code, against the user's own calendar. A phrase can mean more than
//! one thing: on a Sunday, "last Saturday" is yesterday or the week before.
//! Both come back, most likely first, and the caller looks at the user's data
//! before deciding whether it needs to ask.

use std::fmt::Write as _;

use jiff::ToSpan;
use jiff::civil::{Date, Weekday};

/// A run of whole days, both ends included.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Period {
    /// The first day.
    pub first: Date,
    /// The last day, never after today.
    pub last: Date,
    /// How to say it: "Saturday, October 31".
    pub label: String,
}

/// What a phrase may mean, most likely first. More than one is ambiguous.
pub type Alternatives = Vec<Period>;

/// The periods `phrase` may mean, as said on `today`, most likely first, or
/// `None` if it names no time. Only the past is considered.
#[must_use]
pub fn resolve(phrase: &str, today: Date) -> Option<Alternatives> {
    let words = words(phrase);
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    let clamp = |mut p: Period| {
        if p.last > today {
            p.last = today;
        }
        p
    };
    let found = iso(&w, today)
        .or_else(|| relative_day(&w, today))
        .or_else(|| ago(&w, today))
        .or_else(|| past_n(&w, today))
        .or_else(|| weekend(&w, today))
        // An explicit date beats a weekday said with it: "Friday, July 31".
        .or_else(|| single_day(month_day(&w, today)))
        .or_else(|| weekday(&w, today))
        .or_else(|| calendar_unit(&w, today))
        .or_else(|| month_day(&w, today))
        .or_else(|| holiday(&w, today))
        .or_else(|| season(&w, today))
        .or_else(|| recent(&w, today))?;
    let periods: Vec<Period> = found
        .into_iter()
        .filter(|p| p.first <= today)
        .map(clamp)
        .collect();
    (!periods.is_empty()).then_some(periods)
}

/// `found`, if it is exactly one day.
fn single_day(found: Option<Alternatives>) -> Option<Alternatives> {
    found.filter(|f| matches!(f[..], [ref p] if p.first == p.last))
}

/// Lowercased words, ordinals and number words turned into digits.
fn words(phrase: &str) -> Vec<String> {
    phrase
        .to_lowercase()
        .replace(['’', '\''], "")
        .split(|c: char| !c.is_alphanumeric() && c != '-')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let stripped = ["st", "nd", "rd", "th"]
                .iter()
                .find_map(|s| {
                    w.strip_suffix(s)
                        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                })
                .unwrap_or(w);
            number_word(stripped).map_or_else(|| stripped.to_owned(), |n| n.to_string())
        })
        .collect()
}

fn number_word(word: &str) -> Option<u32> {
    const WORDS: &[&str] = &[
        "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
        "eleven", "twelve",
    ];
    match word {
        "a" | "an" | "first" => Some(1),
        "couple" | "second" => Some(2),
        "few" | "third" => Some(3),
        "fourth" => Some(4),
        "fifth" => Some(5),
        _ => WORDS
            .iter()
            .position(|w| *w == word)
            .and_then(|i| u32::try_from(i).ok()),
    }
}

fn number(word: &str) -> Option<u32> {
    word.parse().ok()
}

fn has(w: &[&str], word: &str) -> bool {
    w.contains(&word)
}

/// The index of `a` followed directly by `b`.
fn pair(w: &[&str], a: &str, b: &str) -> Option<usize> {
    w.windows(2).position(|p| p[0] == a && p[1] == b)
}

const MONTHS: &[&str] = &[
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

fn month_of(word: &str) -> Option<i8> {
    let index = MONTHS.iter().position(|m| {
        *m == word || (word.len() >= 3 && word != "may" && m.starts_with(word) && word.len() <= 4)
    })?;
    i8::try_from(index + 1).ok()
}

fn weekday_of(word: &str) -> Option<Weekday> {
    Some(match word {
        // No "sat" or "sun": "we sat in the sun" names no day.
        "monday" => Weekday::Monday,
        "tuesday" | "tues" => Weekday::Tuesday,
        "wednesday" => Weekday::Wednesday,
        "thursday" | "thurs" => Weekday::Thursday,
        "friday" => Weekday::Friday,
        "saturday" => Weekday::Saturday,
        "sunday" => Weekday::Sunday,
        _ => return None,
    })
}

fn month_name(month: i8) -> &'static str {
    let name = MONTHS[usize::try_from(month - 1).unwrap_or(0)];
    // Capitalized for speech: "October".
    match name {
        "january" => "January",
        "february" => "February",
        "march" => "March",
        "april" => "April",
        "may" => "May",
        "june" => "June",
        "july" => "July",
        "august" => "August",
        "september" => "September",
        "october" => "October",
        "november" => "November",
        _ => "December",
    }
}

fn weekday_name(day: Weekday) -> &'static str {
    match day {
        Weekday::Monday => "Monday",
        Weekday::Tuesday => "Tuesday",
        Weekday::Wednesday => "Wednesday",
        Weekday::Thursday => "Thursday",
        Weekday::Friday => "Friday",
        Weekday::Saturday => "Saturday",
        Weekday::Sunday => "Sunday",
    }
}

/// "Saturday, October 31", with the year only if it is not this one.
#[must_use]
pub fn day_label(day: Date, today: Date) -> String {
    let mut label = format!(
        "{}, {} {}",
        weekday_name(day.weekday()),
        month_name(day.month()),
        day.day()
    );
    if day.year() != today.year() {
        let _ = write!(label, ", {}", day.year());
    }
    label
}

fn span_label(first: Date, last: Date, today: Date) -> String {
    if first == last {
        return day_label(first, today);
    }
    let year = |d: Date| {
        if d.year() == today.year() {
            String::new()
        } else {
            format!(", {}", d.year())
        }
    };
    if first.month() == last.month() && first.year() == last.year() {
        format!(
            "{} {} to {}{}",
            month_name(first.month()),
            first.day(),
            last.day(),
            year(last)
        )
    } else {
        format!(
            "{} {}{} to {} {}{}",
            month_name(first.month()),
            first.day(),
            year(first),
            month_name(last.month()),
            last.day(),
            year(last)
        )
    }
}

fn day(d: Date, today: Date) -> Period {
    Period {
        first: d,
        last: d,
        label: day_label(d, today),
    }
}

fn span(first: Date, last: Date, today: Date) -> Period {
    Period {
        first,
        last,
        label: span_label(first, last, today),
    }
}

fn labeled(first: Date, last: Date, label: impl Into<String>) -> Period {
    Period {
        first,
        last,
        label: label.into(),
    }
}

fn minus_days(d: Date, days: i64) -> Date {
    d.checked_sub(days.days()).unwrap_or(d)
}

/// The latest `weekday` on or before `today` (strictly before, unless
/// `include_today`).
fn latest(weekday: Weekday, today: Date, include_today: bool) -> Date {
    let back = i64::from(today.weekday().since(weekday));
    let back = if back == 0 && !include_today { 7 } else { back };
    minus_days(today, back)
}

fn monday_of(d: Date) -> Date {
    latest(Weekday::Monday, d, true)
}

fn iso(w: &[&str], today: Date) -> Option<Alternatives> {
    let dates: Vec<Date> = w.iter().filter_map(|t| t.parse::<Date>().ok()).collect();
    match dates[..] {
        [one] => Some(vec![day(one, today)]),
        [a, b, ..] => Some(vec![span(a.min(b), a.max(b), today)]),
        [] => None,
    }
}

fn relative_day(w: &[&str], today: Date) -> Option<Alternatives> {
    if pair(w, "before", "yesterday").is_some() {
        return Some(vec![day(minus_days(today, 2), today)]);
    }
    if has(w, "yesterday") {
        return Some(vec![day(minus_days(today, 1), today)]);
    }
    let this_part = ["morning", "afternoon", "evening"]
        .iter()
        .any(|part| pair(w, "this", part).is_some());
    if has(w, "today") || has(w, "tonight") || this_part {
        return Some(vec![day(today, today)]);
    }
    None
}

/// "three days ago", "two weeks ago", "a month ago".
fn ago(w: &[&str], today: Date) -> Option<Alternatives> {
    let at = w.iter().position(|t| *t == "ago")?;
    let unit = *w.get(at.checked_sub(1)?)?;
    let n = at
        .checked_sub(2)
        .and_then(|i| w.get(i))
        .and_then(|t| number(t))
        .unwrap_or(1);
    let n = i64::from(n);
    Some(vec![match unit.trim_end_matches('s') {
        "day" => day(minus_days(today, n), today),
        "week" => {
            let monday = monday_of(minus_days(today, 7 * n));
            labeled(
                monday,
                minus_days(monday, -6),
                format!("the week of {}", short_label(monday)),
            )
        }
        "month" => month_period(today.checked_sub((n).months()).ok()?, today),
        "year" => {
            let year = today.year() - i16::try_from(n).ok()?;
            year_period(year, today)?
        }
        _ => return None,
    }])
}

fn short_label(d: Date) -> String {
    format!("{} {}", month_name(d.month()), d.day())
}

/// "the past three days", "last two weeks".
fn past_n(w: &[&str], today: Date) -> Option<Alternatives> {
    let at = w.iter().position(|t| *t == "past" || *t == "last")?;
    let n = number(w.get(at + 1)?)?;
    let unit = w.get(at + 2)?.trim_end_matches('s');
    let days = i64::from(n)
        * match unit {
            "day" => 1,
            "week" => 7,
            "month" => 30,
            _ => return None,
        };
    let first = minus_days(today, days - 1);
    Some(vec![labeled(first, today, format!("the past {n} {unit}s"))])
}

fn weekend(w: &[&str], today: Date) -> Option<Alternatives> {
    let at = w.iter().position(|t| *t == "weekend")?;
    let current = match today.weekday() {
        Weekday::Saturday => today,
        Weekday::Sunday => minus_days(today, 1),
        _ => latest(Weekday::Saturday, today, false),
    };
    let weekend = |saturday: Date| {
        labeled(
            saturday,
            minus_days(saturday, -1),
            format!("the weekend of {}", short_label(saturday)),
        )
    };
    let modifier = at.checked_sub(1).and_then(|i| w.get(i)).copied();
    if matches!(modifier, Some("last" | "past")) {
        // Said on the weekend or the Monday after, "last weekend" may mean
        // the one just gone or the one before.
        let recent = (today - current).get_days() <= 2;
        let previous = minus_days(current, 7);
        return Some(if recent {
            vec![weekend(current), weekend(previous)]
        } else {
            vec![weekend(current)]
        });
    }
    Some(vec![weekend(current)])
}

fn weekday(w: &[&str], today: Date) -> Option<Alternatives> {
    let at = w.iter().position(|t| weekday_of(t).is_some())?;
    let target = weekday_of(w[at])?;
    if pair(&w[at..], "before", "last").is_some() {
        let d = minus_days(latest(target, today, false), 7);
        return Some(vec![day(d, today)]);
    }
    let modifier = at.checked_sub(1).and_then(|i| w.get(i)).copied();
    if matches!(modifier, Some("last" | "past")) {
        let d = latest(target, today, false);
        // Two days back or less, "last Saturday" may mean the week before.
        return Some(if (today - d).get_days() <= 2 {
            vec![day(d, today), day(minus_days(d, 7), today)]
        } else {
            vec![day(d, today)]
        });
    }
    Some(vec![day(latest(target, today, true), today)])
}

fn month_period(any_day: Date, today: Date) -> Period {
    let first = any_day.first_of_month();
    let last = any_day.last_of_month();
    let mut label = month_name(first.month()).to_owned();
    if first.year() != today.year() {
        let _ = write!(label, " {}", first.year());
    }
    labeled(first, last, label)
}

fn year_period(year: i16, today: Date) -> Option<Period> {
    let first = Date::new(year, 1, 1).ok()?;
    let last = Date::new(year, 12, 31).ok()?;
    let label = if year == today.year() {
        "this year".to_owned()
    } else {
        year.to_string()
    };
    Some(labeled(first, last, label))
}

/// "last week", "this month", "last year".
fn calendar_unit(w: &[&str], today: Date) -> Option<Alternatives> {
    for (i, unit) in w.iter().enumerate() {
        let modifier = i.checked_sub(1).and_then(|j| w.get(j)).copied();
        let last = matches!(modifier, Some("last" | "past"));
        let this = matches!(modifier, Some("this"));
        if !last && !this {
            continue;
        }
        let period = match *unit {
            "week" => {
                let monday = monday_of(today);
                if last {
                    let start = minus_days(monday, 7);
                    labeled(
                        start,
                        minus_days(start, -6),
                        format!("the week of {}", short_label(start)),
                    )
                } else {
                    labeled(monday, today, "this week")
                }
            }
            "month" => {
                if last {
                    month_period(today.first_of_month().checked_sub(1.day()).ok()?, today)
                } else {
                    labeled(today.first_of_month(), today, "this month")
                }
            }
            "year" => year_period(today.year() - i16::from(last), today)?,
            _ => continue,
        };
        return Some(vec![period]);
    }
    None
}

/// "October 31", "the 31st of October", "last October", "the 31st".
fn month_day(w: &[&str], today: Date) -> Option<Alternatives> {
    let month_at = w.iter().position(|t| month_of(t).is_some());
    let Some(at) = month_at else {
        // "the 31st": that day of this month, or of the last month that had it.
        let at = w
            .windows(2)
            .position(|p| p[0] == "the" && number(p[1]).is_some())?;
        let n = i8::try_from(number(w[at + 1])?).ok()?;
        let mut month = today.first_of_month();
        for _ in 0..12 {
            if let Ok(d) = Date::new(month.year(), month.month(), n)
                && d <= today
            {
                return Some(vec![day(d, today)]);
            }
            month = month.checked_sub(1.month()).ok()?;
        }
        return None;
    };
    let month = month_of(w[at])?;
    let day_number = w
        .get(at + 1)
        .and_then(|t| number(t))
        .or_else(|| {
            w.get(at + 2)
                .filter(|_| w.get(at + 1) == Some(&"the"))
                .and_then(|t| number(t))
        })
        .or_else(|| at.checked_sub(1).and_then(|i| number(w[i])))
        .or_else(|| {
            (w.get(at.wrapping_sub(1)) == Some(&"of"))
                .then(|| at.checked_sub(2).and_then(|i| number(w[i])))
                .flatten()
        })
        .filter(|n| (1..=31).contains(n));
    let explicit_year = w
        .iter()
        .filter_map(|t| number(t))
        .find(|n| (1900..=2200).contains(n))
        .and_then(|y| i16::try_from(y).ok());
    let modifier = at.checked_sub(1).and_then(|i| w.get(i)).copied();
    let year = explicit_year.unwrap_or_else(|| {
        let this_year = today.year();
        let passed = (
            month,
            day_number.map_or(1, |d| i8::try_from(d).unwrap_or(1)),
        ) <= (today.month(), today.day());
        let last = matches!(modifier, Some("last" | "past"));
        if !passed || (last && month == today.month()) {
            this_year - 1
        } else {
            this_year
        }
    });
    Some(vec![match day_number {
        Some(n) => day(Date::new(year, month, i8::try_from(n).ok()?).ok()?, today),
        None => month_period(Date::new(year, month, 1).ok()?, today),
    }])
}

/// The most recent occurrence of a fixed-date period that has begun.
fn latest_fixed(today: Date, month: i8, day_number: i8, len: i64, label: &str) -> Option<Period> {
    let mut year = today.year();
    loop {
        let first = Date::new(year, month, day_number).ok()?;
        if first <= today {
            let last = minus_days(first, -(len - 1));
            let mut label = label.to_owned();
            if first.year() != today.year() {
                let _ = write!(label, " {}", first.year());
            }
            return Some(labeled(first, last, label));
        }
        year -= 1;
    }
}

fn holiday(w: &[&str], today: Date) -> Option<Alternatives> {
    let joined = w.join(" ");
    let period = if has(w, "christmas") || has(w, "xmas") {
        latest_fixed(today, 12, 24, 3, "Christmas")?
    } else if joined.contains("new year") {
        latest_fixed(today, 12, 31, 2, "New Year's")?
    } else if has(w, "halloween") {
        latest_fixed(today, 10, 31, 1, "Halloween")?
    } else if has(w, "thanksgiving") {
        // The fourth Thursday of November, and the long weekend after it.
        let mut year = today.year();
        loop {
            let november = Date::new(year, 11, 1).ok()?;
            let thursday = november.nth_weekday_of_month(4, Weekday::Thursday).ok()?;
            if thursday <= today {
                let label = if year == today.year() {
                    "Thanksgiving".to_owned()
                } else {
                    format!("Thanksgiving {year}")
                };
                break labeled(thursday, minus_days(thursday, -3), label);
            }
            year -= 1;
        }
    } else {
        return None;
    };
    Some(vec![period])
}

fn season(w: &[&str], today: Date) -> Option<Alternatives> {
    let (start_month, name) = if has(w, "summer") {
        (6, "summer")
    } else if has(w, "spring") {
        (3, "spring")
    } else if has(w, "fall") || has(w, "autumn") {
        (9, "fall")
    } else if has(w, "winter") {
        (12, "winter")
    } else {
        return None;
    };
    let last = w
        .windows(2)
        .any(|p| matches!(p[0], "last" | "past") && p[1] == name);
    let mut year = today.year();
    let mut skip = last;
    loop {
        let first = Date::new(year, start_month, 1).ok()?;
        let end = first
            .checked_add(3.months())
            .ok()?
            .checked_sub(1.day())
            .ok()?;
        if first <= today {
            // "Last summer", said in summer, is the one before.
            if skip && end >= today {
                skip = false;
                year -= 1;
                continue;
            }
            let label = if first.year() == today.year()
                || (start_month == 12 && first.year() + 1 == today.year())
            {
                format!("this {name}")
            } else {
                format!("{name} {}", first.year())
            };
            let label = if end < today && label.starts_with("this") {
                format!("{name} {}", first.year())
            } else {
                label
            };
            return Some(vec![labeled(first, end, label)]);
        }
        year -= 1;
    }
}

fn recent(w: &[&str], today: Date) -> Option<Alternatives> {
    let recent = ["recent", "recently", "lately", "latest", "newest", "new"];
    if w.iter().any(|t| recent.contains(t)) || pair(w, "last", "few").is_some() {
        return Some(vec![labeled(
            minus_days(today, 13),
            today,
            "the past two weeks",
        )]);
    }
    None
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;

    use super::*;

    /// A Sunday.
    const TODAY: Date = date(2026, 11, 1);

    fn days(phrase: &str) -> Vec<(Date, Date)> {
        resolve(phrase, TODAY)
            .unwrap_or_else(|| panic!("{phrase:?} not understood"))
            .into_iter()
            .map(|p| (p.first, p.last))
            .collect()
    }

    fn one(d: Date) -> Vec<(Date, Date)> {
        vec![(d, d)]
    }

    #[test]
    fn last_saturday_on_a_sunday_is_ambiguous() {
        let periods = resolve("our vacation from last Saturday", TODAY).unwrap();
        assert_eq!(
            periods.iter().map(|p| p.label.as_str()).collect::<Vec<_>>(),
            ["Saturday, October 31", "Saturday, October 24"]
        );
        // Midweek, it is not.
        assert_eq!(
            resolve("last saturday", date(2026, 11, 4)).unwrap().len(),
            1
        );
    }

    #[test]
    fn a_date_said_with_its_weekday_is_that_date() {
        assert_eq!(days("Friday, July 31"), one(date(2026, 7, 31)));
        assert_eq!(days("Friday July 31st"), one(date(2026, 7, 31)));
        // A month alone does not override the weekday.
        assert_eq!(days("on Saturday"), one(date(2026, 10, 31)));
    }

    #[test]
    fn weekdays() {
        assert_eq!(days("on Saturday"), one(date(2026, 10, 31)));
        assert_eq!(days("sunday"), one(TODAY));
        assert_eq!(days("last Sunday"), one(date(2026, 10, 25)));
        assert_eq!(days("Tuesday"), one(date(2026, 10, 27)));
        assert_eq!(days("last tuesday"), one(date(2026, 10, 27)));
        assert_eq!(days("the Saturday before last"), one(date(2026, 10, 24)));
    }

    #[test]
    fn relative_days() {
        assert_eq!(days("today"), one(TODAY));
        assert_eq!(days("this morning"), one(TODAY));
        assert_eq!(days("yesterday"), one(date(2026, 10, 31)));
        assert_eq!(days("the day before yesterday"), one(date(2026, 10, 30)));
        assert_eq!(days("three days ago"), one(date(2026, 10, 29)));
        assert_eq!(days("2 days ago"), one(date(2026, 10, 30)));
        assert_eq!(
            days("two weeks ago"),
            vec![(date(2026, 10, 12), date(2026, 10, 18))]
        );
        assert_eq!(
            days("the past three days"),
            vec![(date(2026, 10, 30), TODAY)]
        );
    }

    #[test]
    fn weeks_weekends_months_years() {
        assert_eq!(
            days("last week"),
            vec![(date(2026, 10, 19), date(2026, 10, 25))]
        );
        assert_eq!(days("this week"), vec![(date(2026, 10, 26), TODAY)]);
        assert_eq!(days("the weekend"), vec![(date(2026, 10, 31), TODAY)]);
        assert_eq!(
            days("last weekend"),
            vec![
                (date(2026, 10, 31), TODAY),
                (date(2026, 10, 24), date(2026, 10, 25))
            ]
        );
        assert_eq!(
            days("last month"),
            vec![(date(2026, 10, 1), date(2026, 10, 31))]
        );
        assert_eq!(days("this month"), vec![(TODAY, TODAY)]);
        assert_eq!(
            days("last year"),
            vec![(date(2025, 1, 1), date(2025, 12, 31))]
        );
    }

    #[test]
    fn calendar_dates() {
        assert_eq!(days("October 31st"), one(date(2026, 10, 31)));
        assert_eq!(days("the 24th of October"), one(date(2026, 10, 24)));
        assert_eq!(days("oct 3"), one(date(2026, 10, 3)));
        // A date later this year has not happened yet: last year's.
        assert_eq!(days("December 20"), one(date(2025, 12, 20)));
        assert_eq!(days("the 15th"), one(date(2026, 10, 15)));
        assert_eq!(days("August"), vec![(date(2026, 8, 1), date(2026, 8, 31))]);
        assert_eq!(
            days("May 2024"),
            vec![(date(2024, 5, 1), date(2024, 5, 31))]
        );
        assert_eq!(days("2026-08-12"), one(date(2026, 8, 12)));
        assert_eq!(
            days("2026-08-10 to 2026-08-12"),
            vec![(date(2026, 8, 10), date(2026, 8, 12))]
        );
    }

    #[test]
    fn holidays_and_seasons() {
        assert_eq!(
            days("Christmas"),
            vec![(date(2025, 12, 24), date(2025, 12, 26))]
        );
        assert_eq!(days("halloween"), one(date(2026, 10, 31)));
        assert_eq!(
            days("thanksgiving"),
            vec![(date(2025, 11, 27), date(2025, 11, 30))]
        );
        assert_eq!(
            days("this summer"),
            vec![(date(2026, 6, 1), date(2026, 8, 31))]
        );
        assert_eq!(
            days("last winter"),
            vec![(date(2025, 12, 1), date(2026, 2, 28))]
        );
    }

    #[test]
    fn labels_are_speakable() {
        let label = |p: &str| resolve(p, TODAY).unwrap()[0].label.clone();
        assert_eq!(label("yesterday"), "Saturday, October 31");
        assert_eq!(label("December 20"), "Saturday, December 20, 2025");
        assert_eq!(label("last week"), "the week of October 19");
        assert_eq!(label("the past 3 days"), "the past 3 days");
        assert_eq!(label("2026-08-10 to 2026-08-12"), "August 10 to 12");
    }

    #[test]
    fn never_the_future_and_none_without_a_time() {
        for phrase in ["a picture of the dog", "", "my son"] {
            assert_eq!(resolve(phrase, TODAY), None, "{phrase:?}");
        }
        for p in ["this week", "the weekend", "this month", "this year"] {
            assert!(resolve(p, TODAY).unwrap().iter().all(|p| p.last <= TODAY));
        }
    }
}
