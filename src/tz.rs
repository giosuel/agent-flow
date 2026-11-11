//! Times in the owner's own time zone (the owner, 2026-11-11: "everything the server sends is
//! timezone-agnostic and the browser then decides").
//!
//! The browser decides: app.js keeps the `af_tz` cookie set to its IANA zone name, and pages,
//! chart days and their labels use that zone. Without the cookie (a first visit, curl) they are
//! UTC. What agents, the API and exports get stays UTC, as RFC 3339 (`util::fmt_utc`).

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan, Zoned};

pub const COOKIE: &str = "af_tz";

#[derive(Clone, Debug)]
pub struct Tz(TimeZone);

impl Default for Tz {
    fn default() -> Self {
        Tz(TimeZone::UTC)
    }
}

impl Tz {
    /// A zone by its IANA name ("Europe/Zurich"), from the system's time zone database.
    pub fn named(name: &str) -> Option<Self> {
        let plausible = !name.is_empty()
            && name.len() <= 64
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"/_+-".contains(&b))
            && !name.contains("..");
        plausible.then(|| TimeZone::get(name).ok()).flatten().map(Tz)
    }

    /// "Europe/Zurich", or "UTC".
    pub fn name(&self) -> &str {
        self.0.iana_name().unwrap_or("UTC")
    }

    fn at(&self, ms: i64) -> Zoned {
        Timestamp::from_millisecond(ms).unwrap_or(Timestamp::UNIX_EPOCH).to_zoned(self.0.clone())
    }

    /// "2026-11-11 02:30:05".
    pub fn full(&self, ms: i64) -> String {
        self.at(ms).strftime("%Y-%m-%d %H:%M:%S").to_string()
    }

    /// "11 Nov 02:04": unambiguous, short, for histories and lists.
    pub fn short(&self, ms: i64) -> String {
        self.at(ms).strftime("%-d %b %H:%M").to_string()
    }

    /// "Wednesday 11 November · 02:04", the Dashboard's line.
    pub fn long(&self, ms: i64) -> String {
        self.at(ms).strftime("%A %-d %B · %H:%M").to_string()
    }

    /// "02:04".
    pub fn hm(&self, ms: i64) -> String {
        self.at(ms).strftime("%H:%M").to_string()
    }

    /// "2026-11-06".
    pub fn date(&self, ms: i64) -> String {
        self.at(ms).strftime("%Y-%m-%d").to_string()
    }

    /// "Mon 10", a day's bar label.
    pub fn day_label(&self, ms: i64) -> String {
        self.at(ms).strftime("%a %-d").to_string()
    }

    /// The start of the local day `ms` falls in.
    pub fn day_start(&self, ms: i64) -> i64 {
        self.at(ms).start_of_day().map(|z| z.timestamp().as_millisecond()).unwrap_or(ms)
    }

    /// The start of a local date ("2026-11-10").
    pub fn parse_day(&self, s: &str) -> Option<i64> {
        let d: Date = s.parse().ok()?;
        Some(d.to_zoned(self.0.clone()).ok()?.start_of_day().ok()?.timestamp().as_millisecond())
    }

    /// The starts of `n` local days ending with the one `ms` falls in, and the end of that day:
    /// `n + 1` instants. Days are 23 or 25 hours long when the clocks change.
    pub fn days(&self, ms: i64, n: usize) -> Vec<i64> {
        let today = self.at(ms).date();
        (0..=n as i64)
            .map(|i| {
                let d = today.checked_add((i - n as i64 + 1).days()).unwrap_or(today);
                d.to_zoned(self.0.clone())
                    .and_then(|z| z.start_of_day())
                    .map(|z| z.timestamp().as_millisecond())
                    .unwrap_or(ms)
            })
            .collect()
    }

    /// The hours of the local day starting at `day`, and its end: usually 25 instants.
    pub fn hours(&self, day: i64) -> Vec<i64> {
        let end = self.days(day, 1)[1];
        let mut out: Vec<i64> = (0..).map(|h| day + h * 3_600_000).take_while(|t| *t < end).collect();
        out.push(end);
        out
    }
}

/// Which of the buckets `bounds` (n + 1 ascending instants) holds `at`.
pub fn bucket(bounds: &[i64], at: i64) -> Option<usize> {
    let i = bounds.partition_point(|b| *b <= at);
    (i >= 1 && i < bounds.len()).then(|| i - 1)
}

impl<S: Send + Sync> FromRequestParts<S> for Tz {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let jar = axum_extra::extract::CookieJar::from_headers(&parts.headers);
        Ok(jar.get(COOKIE).and_then(|c| Tz::named(c.value())).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-03-29 is the day Zurich moves its clocks forward (02:00 → 03:00).
    const SPRING: i64 = 1_774_778_400_000; // 2026-03-29T10:00:00Z

    #[test]
    fn local_days_and_labels() {
        let z = Tz::named("Europe/Zurich").unwrap();
        assert_eq!(z.name(), "Europe/Zurich");
        let night = 1_794_355_200_000 - 1; // 2026-11-10T23:59:59.999Z, already the 11th in Zurich
        assert_eq!(z.date(night), "2026-11-11");
        assert_eq!(Tz::default().date(night), "2026-11-10");
        assert_eq!(z.full(night), "2026-11-11 00:59:59");
        assert_eq!(z.short(night), "11 Nov 00:59");
        assert_eq!(z.day_label(night), "Wed 11");
        assert_eq!(z.day_start(night), 1_794_351_600_000); // 2026-11-10T23:00:00Z
        assert_eq!(z.parse_day("2026-11-11"), Some(1_794_351_600_000));
        let days = z.days(SPRING, 2);
        assert_eq!(days.len(), 3);
        assert_eq!(days[2] - days[1], 23 * 3_600_000, "the spring day is 23 hours");
        assert_eq!(z.hours(days[1]).len(), 24);
        assert_eq!(bucket(&days, days[1]), Some(1));
        assert_eq!(bucket(&days, days[2]), None);
        assert_eq!(bucket(&days, days[0] - 1), None);
    }

    #[test]
    fn odd_names_fall_back() {
        assert!(Tz::named("../../etc/passwd").is_none());
        assert!(Tz::named("Not/AZone").is_none());
        assert!(Tz::named("").is_none());
        assert_eq!(crate::util::fmt_utc(1_794_355_199_999), "2026-11-10T23:59:59Z");
    }
}
