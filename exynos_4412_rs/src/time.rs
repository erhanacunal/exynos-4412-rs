//! Calendar dates and times, and their conversion to and from seconds since 1970.
//!
//! The day count follows Howard Hinnant's `days_from_civil` and `civil_from_days`
//! (<https://howardhinnant.github.io/date_algorithms.html>), proleptic Gregorian calendar.

use core::fmt;

/// A date and time to the second, with no time zone of its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct DateTime {
    /// The year, such as 2026.
    pub year: u16,
    /// 1 to 12.
    pub month: u8,
    /// 1 to the length of the month.
    pub day: u8,
    /// 0 to 23.
    pub hour: u8,
    /// 0 to 59.
    pub minute: u8,
    /// 0 to 59.
    pub second: u8,
}

const fn is_leap(year: u16) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

/// Days in `month` of `year`, 0 for a month that does not exist.
pub const fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

impl DateTime {
    /// Whether every field is in its range, the day included.
    pub const fn is_valid(&self) -> bool {
        self.month >= 1
            && self.month <= 12
            && self.day >= 1
            && self.day <= days_in_month(self.year, self.month)
            && self.hour < 24
            && self.minute < 60
            && self.second < 60
    }

    /// Seconds since 1970-01-01 00:00:00. Meaningful only for a valid date.
    pub const fn to_unix(&self) -> i64 {
        let (month, day) = (self.month as i64, self.day as i64);
        let year = self.year as i64 - if month <= 2 { 1 } else { 0 };
        let era = year.div_euclid(400);
        let year_of_era = year - era * 400;
        let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        let days = era * 146_097 + day_of_era - 719_468;
        days * 86_400 + self.hour as i64 * 3600 + self.minute as i64 * 60 + self.second as i64
    }

    /// The date and time `seconds` after 1970-01-01 00:00:00, for years 0 to 65535.
    pub const fn from_unix(seconds: i64) -> Self {
        let days = seconds.div_euclid(86_400);
        let of_day = seconds.rem_euclid(86_400);
        let shifted = days + 719_468;
        let era = shifted.div_euclid(146_097);
        let day_of_era = shifted - era * 146_097;
        let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let month_index = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * month_index + 2) / 5 + 1;
        let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
        let year = year_of_era + era * 400 + if month <= 2 { 1 } else { 0 };
        Self {
            year: year as u16,
            month: month as u8,
            day: day as u8,
            hour: (of_day / 3600) as u8,
            minute: (of_day / 60 % 60) as u8,
            second: (of_day % 60) as u8,
        }
    }

    /// The same moment `seconds` later, or earlier for a negative count.
    pub const fn add_seconds(&self, seconds: i64) -> Self {
        Self::from_unix(self.to_unix() + seconds)
    }
}

impl fmt::Display for DateTime {
    /// `YYYY-MM-DD HH:MM:SS`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}
