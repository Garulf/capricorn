//! Age and size limits over hour segments.

use capricorn_core::proto::Ts;

use crate::segment::{SegmentInfo, hour_end_ms};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// Hours to remove so that no sealed hour is older than `max_age_days` and the total size
/// fits in `max_bytes`, removing the oldest first. Hours that are not sealed are never
/// chosen but still count toward the size. Returned in ascending order.
pub fn plan_retention(
    hours: &[SegmentInfo],
    now: Ts,
    max_age_days: Option<u32>,
    max_bytes: Option<u64>,
) -> Vec<u32> {
    let mut sealed: Vec<&SegmentInfo> = hours.iter().filter(|info| info.sealed).collect();
    sealed.sort_by_key(|info| info.hour);
    let too_old = |info: &SegmentInfo| {
        max_age_days.is_some_and(|days| hour_end_ms(info.hour) <= now - i64::from(days) * DAY_MS)
    };
    let mut total = hours
        .iter()
        .fold(0u64, |sum, info| sum.saturating_add(info.bytes));
    let mut removed = Vec::new();
    for info in sealed {
        let over_cap = max_bytes.is_some_and(|cap| total > cap);
        if too_old(info) || over_cap {
            removed.push(info.hour);
            total = total.saturating_sub(info.bytes);
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::{HOUR_MS, hour_start_ms};

    const NOW_HOUR: u32 = 480_000;
    const DAY_HOURS: u32 = 24;

    fn now() -> Ts {
        hour_start_ms(NOW_HOUR) + HOUR_MS / 2
    }

    fn sealed(hour: u32, bytes: u64) -> SegmentInfo {
        SegmentInfo {
            hour,
            sealed: true,
            bytes,
        }
    }

    fn open(hour: u32, bytes: u64) -> SegmentInfo {
        SegmentInfo {
            hour,
            sealed: false,
            bytes,
        }
    }

    fn days_ago(days: u32) -> u32 {
        NOW_HOUR - days * DAY_HOURS
    }

    #[test]
    fn age_limit_removes_old() {
        let hours = [
            sealed(days_ago(100), 10),
            sealed(days_ago(91), 10),
            sealed(days_ago(90) - 1, 10),
            sealed(days_ago(90), 10),
            sealed(days_ago(3), 10),
        ];
        let removed = plan_retention(&hours, now(), Some(90), None);
        assert_eq!(removed, vec![days_ago(100), days_ago(91), days_ago(90) - 1]);
    }

    #[test]
    fn size_cap_removes_oldest_first() {
        let hours = [
            sealed(NOW_HOUR - 4, 100),
            sealed(NOW_HOUR - 3, 100),
            sealed(NOW_HOUR - 2, 100),
            sealed(NOW_HOUR - 1, 100),
        ];
        assert_eq!(
            plan_retention(&hours, now(), None, Some(250)),
            vec![NOW_HOUR - 4, NOW_HOUR - 3]
        );
        assert_eq!(
            plan_retention(&hours, now(), None, Some(400)),
            Vec::<u32>::new()
        );
        assert_eq!(
            plan_retention(&hours, now(), None, Some(399)),
            vec![NOW_HOUR - 4]
        );
    }

    #[test]
    fn size_cap_ignores_input_order() {
        let hours = [
            sealed(NOW_HOUR - 1, 100),
            sealed(NOW_HOUR - 5, 100),
            sealed(NOW_HOUR - 3, 100),
        ];
        assert_eq!(
            plan_retention(&hours, now(), None, Some(150)),
            vec![NOW_HOUR - 5, NOW_HOUR - 3]
        );
    }

    #[test]
    fn both_limits_union() {
        let hours = [
            sealed(days_ago(200), 10),
            sealed(NOW_HOUR - 3, 100),
            sealed(NOW_HOUR - 2, 100),
            sealed(NOW_HOUR - 1, 100),
        ];
        let removed = plan_retention(&hours, now(), Some(90), Some(150));
        assert_eq!(removed, vec![days_ago(200), NOW_HOUR - 3, NOW_HOUR - 2]);
    }

    #[test]
    fn none_means_forever() {
        let hours = [
            sealed(0, u64::MAX / 4),
            sealed(days_ago(10_000), u64::MAX / 4),
            sealed(NOW_HOUR - 1, 1),
        ];
        assert!(plan_retention(&hours, now(), None, None).is_empty());
    }

    #[test]
    fn open_hours_never_removed() {
        let hours = [
            open(days_ago(400), 1_000),
            sealed(NOW_HOUR - 2, 10),
            open(NOW_HOUR, 1_000),
        ];
        assert_eq!(
            plan_retention(&hours, now(), Some(1), Some(0)),
            vec![NOW_HOUR - 2]
        );
    }

    #[test]
    fn zero_age_removes_every_sealed_hour_that_ended() {
        let hours = [sealed(NOW_HOUR - 1, 1), sealed(NOW_HOUR, 1)];
        assert_eq!(
            plan_retention(&hours, now(), Some(0), None),
            vec![NOW_HOUR - 1]
        );
    }
}
