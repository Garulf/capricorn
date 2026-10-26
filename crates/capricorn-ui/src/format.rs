use capricorn_core::proto::Ts;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};

/// Wall clock used by every view, fixed in tests so snapshots are stable.
#[derive(Clone, Debug)]
pub struct Clock {
    pub now: Ts,
    pub tz: TimeZone,
}

impl Clock {
    pub fn system() -> Self {
        Self {
            now: Timestamp::now().as_millisecond(),
            tz: TimeZone::system(),
        }
    }

    pub fn zoned(&self, ts: Ts) -> Zoned {
        let stamp = Timestamp::from_millisecond(ts).unwrap_or(Timestamp::UNIX_EPOCH);
        stamp.to_zoned(self.tz.clone())
    }

    pub fn today(&self) -> Date {
        self.zoned(self.now).date()
    }

    /// Start of `day` and start of the next day, in unix milliseconds.
    pub fn day_bounds(&self, day: Date) -> (Ts, Ts) {
        let start = self.day_start(day);
        let end = day
            .tomorrow()
            .map_or(start + DAY_MS, |next| self.day_start(next));
        (start, end)
    }

    fn day_start(&self, day: Date) -> Ts {
        day.to_zoned(self.tz.clone())
            .map_or(0, |z| z.timestamp().as_millisecond())
    }
}

const DAY_MS: Ts = 24 * 3600 * 1000;
const RECENT_DAYS: i32 = 6;

/// "today 14:32" for today, "Sat 09:10" within the last week, "12 Oct" before that.
pub fn relative_time(ts: Ts, clock: &Clock) -> String {
    let when = clock.zoned(ts);
    let today = clock.today();
    let date = when.date();
    if date == today {
        return when.strftime("today %H:%M").to_string();
    }
    let days_ago = (today - date).get_days();
    if (1..=RECENT_DAYS).contains(&days_ago) {
        return when.strftime("%a %H:%M").to_string();
    }
    if date.year() == today.year() {
        when.strftime("%-d %b").to_string()
    } else {
        when.strftime("%-d %b %Y").to_string()
    }
}

pub fn clock_time(ts: Ts, clock: &Clock) -> String {
    clock.zoned(ts).strftime("%H:%M:%S").to_string()
}

pub fn hour_label(ts: Ts, clock: &Clock) -> String {
    clock.zoned(ts).strftime("%H:%M").to_string()
}

pub fn day_label(day: Date) -> String {
    day.strftime("%a %-d %b").to_string()
}

/// "locks in 14m", or seconds in the last minute.
pub fn lock_countdown(locks_in_secs: u64) -> String {
    if locks_in_secs < 60 {
        format!("locks in {locks_in_secs}s")
    } else {
        format!("locks in {}m", locks_in_secs.div_ceil(60))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    fn clock_at(year: i16, month: i8, day: i8, hour: i8, minute: i8) -> Clock {
        let tz = TimeZone::fixed(jiff::tz::offset(2));
        let now = date(year, month, day)
            .at(hour, minute, 0, 0)
            .to_zoned(tz.clone())
            .unwrap()
            .timestamp()
            .as_millisecond();
        Clock { now, tz }
    }

    fn ts(clock: &Clock, year: i16, month: i8, day: i8, hour: i8, minute: i8) -> Ts {
        date(year, month, day)
            .at(hour, minute, 0, 0)
            .to_zoned(clock.tz.clone())
            .unwrap()
            .timestamp()
            .as_millisecond()
    }

    #[test]
    fn relative_time_format() {
        let clock = clock_at(2026, 10, 27, 18, 0);
        assert_eq!(
            relative_time(ts(&clock, 2026, 10, 27, 14, 32), &clock),
            "today 14:32"
        );
        assert_eq!(
            relative_time(ts(&clock, 2026, 10, 24, 9, 10), &clock),
            "Sat 09:10"
        );
        assert_eq!(
            relative_time(ts(&clock, 2026, 10, 12, 9, 10), &clock),
            "12 Oct"
        );
        assert_eq!(
            relative_time(ts(&clock, 2025, 12, 30, 9, 10), &clock),
            "30 Dec 2025"
        );
    }

    #[test]
    fn relative_time_uses_local_midnight() {
        let clock = clock_at(2026, 10, 27, 0, 30);
        assert_eq!(
            relative_time(ts(&clock, 2026, 10, 26, 23, 50), &clock),
            "Mon 23:50"
        );
    }

    #[test]
    fn lock_countdown_rounds_up_minutes() {
        assert_eq!(lock_countdown(14 * 60), "locks in 14m");
        assert_eq!(lock_countdown(13 * 60 + 1), "locks in 14m");
        assert_eq!(lock_countdown(42), "locks in 42s");
    }

    #[test]
    fn day_bounds_span_one_local_day() {
        let clock = clock_at(2026, 10, 27, 12, 0);
        let (start, end) = clock.day_bounds(date(2026, 10, 27));
        assert_eq!(start, ts(&clock, 2026, 10, 27, 0, 0));
        assert_eq!(end - start, DAY_MS);
        assert_eq!(day_label(date(2026, 10, 27)), "Tue 27 Oct");
    }
}
