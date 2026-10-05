//! Purchase-interval suggestions (issue #106).
//!
//! BuyWise learns what the household buys regularly from check-offs: every
//! resolved item that is checked off records purchase evidence (see
//! `migrations/013_purchase_suggestions.sql`). This module turns that raw
//! evidence into a small, transparent statistic — no AI, no model.
//!
//! For one canonical item:
//!
//! 1. Collapse the evidence to the set of DISTINCT purchase days (two check-offs
//!    on the same day are one purchase).
//! 2. Require a minimum number of purchases ([`MIN_PURCHASE_DAYS`]) before an item
//!    is ever suggested; below that the interval estimate is not trusted.
//! 3. Estimate the typical interval as the MEDIAN gap in days between consecutive
//!    purchase days.
//! 4. Suggest the item once the gap since the last purchase reaches
//!    [`DUE_RATIO`] of that typical interval (the "window"), i.e. shortly before
//!    the household would normally run out.
//!
//! Dismissals push the next suggestion later: an item dismissed on day D is
//! suppressed until one typical interval after D, so the household sees it again
//! next cycle rather than on every page load.
//!
//! A suggestion is only ever a hint. The item is not added to a list until the
//! user confirms it, and a due date is never taken as proof that stock has run
//! out (the household may have bought it elsewhere). The refinement in issue #106
//! is explicit about this: check-offs are a proxy for purchases, distinct from
//! receipt-confirmed purchases.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// Minimum number of distinct purchase days before an item is suggested. Below
/// this the median interval is too noisy to act on.
pub const MIN_PURCHASE_DAYS: usize = 3;

/// Fraction of the typical interval at which an item becomes due: 4/5 = 80%.
/// A suggestion appears once `days_since_last >= interval * DUE_NUM / DUE_DEN`.
pub const DUE_NUM: i64 = 4;
pub const DUE_DEN: i64 = 5;

/// One observed purchase of a canonical item on a particular UTC calendar day.
///
/// The source (check-off vs a future receipt/import) is intentionally not carried
/// here: the computation collapses same-day rows regardless of source, which is
/// what keeps a later receipt import from double counting a purchase the
/// check-off already recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurchaseDay {
    pub canonical_item_id: String,
    pub day: NaiveDate,
}

/// A user dismissal of a suggestion for one canonical item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dismissal {
    pub canonical_item_id: String,
    pub day: NaiveDate,
}

/// A canonical item the household is likely to need soon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suggestion {
    /// `canonical_items.id`.
    pub canonical_item_id: String,
    /// Median gap, in days, between consecutive distinct purchase days.
    pub typical_interval_days: i64,
    /// The most recent purchase day the estimate is based on.
    pub last_purchased_on: NaiveDate,
    /// Days between `last_purchased_on` and the day the suggestion was computed.
    pub days_since_last_purchase: i64,
}

/// Median gap in days between consecutive DISTINCT days in `days`.
///
/// Returns `None` when fewer than two distinct days are supplied (no interval to
/// measure). Duplicate days collapse first, so same-day check-offs never produce a
/// zero interval. An even number of intervals takes the mean of the two middle
/// values (floor), keeping the result an integer number of days.
pub fn median_interval_days(days: &[NaiveDate]) -> Option<i64> {
    let mut sorted: Vec<NaiveDate> = days.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.len() < 2 {
        return None;
    }
    let intervals: Vec<i64> = sorted
        .windows(2)
        .map(|w| (w[1] - w[0]).num_days())
        .collect();
    Some(median(&intervals))
}

/// True when `days_since_last` has reached the due window of `interval_days`.
pub fn is_due(days_since_last: i64, interval_days: i64) -> bool {
    days_since_last >= 0
        && interval_days > 0
        && days_since_last * DUE_DEN >= interval_days * DUE_NUM
}

/// Compute the suggestions for one household on `today`.
///
/// * `evidence` — every purchase day for the household (all canonical items).
/// * `dismissals` — every dismissal; only the most recent per item is used.
/// * `excluded_item_ids` — canonical items already on the list, which must never
///   be suggested.
///
/// Results are ordered most-overdue first (`days_since_last - interval`), with the
/// canonical item id as a deterministic tie-break.
pub fn compute_suggestions(
    evidence: &[PurchaseDay],
    dismissals: &[Dismissal],
    excluded_item_ids: &HashSet<String>,
    today: NaiveDate,
) -> Vec<Suggestion> {
    let mut by_item: BTreeMap<&str, BTreeSet<NaiveDate>> = BTreeMap::new();
    for event in evidence {
        by_item
            .entry(event.canonical_item_id.as_str())
            .or_default()
            .insert(event.day);
    }

    let mut latest_dismissal: HashMap<&str, NaiveDate> = HashMap::new();
    for dismissal in dismissals {
        latest_dismissal
            .entry(dismissal.canonical_item_id.as_str())
            .and_modify(|day| {
                if dismissal.day > *day {
                    *day = dismissal.day;
                }
            })
            .or_insert(dismissal.day);
    }

    let mut suggestions = Vec::new();
    for (item_id, day_set) in by_item {
        if excluded_item_ids.contains(item_id) {
            continue;
        }
        let days: Vec<NaiveDate> = day_set.into_iter().collect();
        if days.len() < MIN_PURCHASE_DAYS {
            continue;
        }
        let Some(interval) = median_interval_days(&days) else {
            continue;
        };
        let last = *days.last().expect("non-empty after min-history check");
        let days_since = (today - last).num_days();
        // Future-dated evidence (clock skew, a correction) is not due yet.
        if days_since < 0 {
            continue;
        }
        // A dismissal suppresses until one typical interval has elapsed since it.
        if let Some(&dismissed_on) = latest_dismissal.get(item_id) {
            if (today - dismissed_on).num_days() < interval {
                continue;
            }
        }
        if is_due(days_since, interval) {
            suggestions.push(Suggestion {
                canonical_item_id: item_id.to_string(),
                typical_interval_days: interval,
                last_purchased_on: last,
                days_since_last_purchase: days_since,
            });
        }
    }

    suggestions.sort_by(|a, b| {
        let overdue_a = a.days_since_last_purchase - a.typical_interval_days;
        let overdue_b = b.days_since_last_purchase - b.typical_interval_days;
        overdue_b
            .cmp(&overdue_a)
            .then_with(|| a.canonical_item_id.cmp(&b.canonical_item_id))
    });
    suggestions
}

/// Median of a non-empty, unsorted slice of integers (even length → floor mean).
fn median(values: &[i64]) -> i64 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    fn evidence(item: &str, days: &[NaiveDate]) -> Vec<PurchaseDay> {
        days.iter()
            .map(|day| PurchaseDay {
                canonical_item_id: item.to_string(),
                day: *day,
            })
            .collect()
    }

    /// Fewer than three distinct purchase days: no suggestion, however overdue.
    #[test]
    fn insufficient_history_gives_no_suggestion() {
        let today = date(2026, 6, 30);
        let ev = evidence("milk", &[date(2026, 6, 1), date(2026, 6, 15)]);
        assert!(compute_suggestions(&ev, &[], &HashSet::new(), today).is_empty());
    }

    /// Same-day duplicates collapse: three DISTINCT days are required, not three rows.
    #[test]
    fn same_day_duplicates_do_not_count_as_history() {
        let today = date(2026, 6, 30);
        let ev = evidence(
            "milk",
            &[
                date(2026, 6, 15),
                date(2026, 6, 15),
                date(2026, 6, 15),
                date(2026, 6, 1),
            ],
        );
        assert!(compute_suggestions(&ev, &[], &HashSet::new(), today).is_empty());
    }

    /// Median of an even number of intervals is the floor mean of the middle two.
    #[test]
    fn median_uses_middle_interval() {
        // Intervals: 2, 4, 10 -> median 4.
        let days = [
            date(2026, 1, 1),
            date(2026, 1, 3),
            date(2026, 1, 7),
            date(2026, 1, 17),
        ];
        assert_eq!(median_interval_days(&days), Some(4));
        // Intervals: 2, 4 -> floor mean of 2 and 4 = 3.
        let days = [date(2026, 1, 1), date(2026, 1, 3), date(2026, 1, 7)];
        assert_eq!(median_interval_days(&days), Some(3));
    }

    /// A weekly staple bought 7/14/21 days ago is due at 7 days since the last.
    #[test]
    fn regular_item_is_due_near_its_usual_time() {
        let today = date(2026, 6, 30);
        let ev = evidence(
            "milk",
            &[date(2026, 6, 9), date(2026, 6, 16), date(2026, 6, 23)],
        );
        let out = compute_suggestions(&ev, &[], &HashSet::new(), today);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].canonical_item_id, "milk");
        assert_eq!(out[0].typical_interval_days, 7);
        assert_eq!(out[0].days_since_last_purchase, 7);
    }

    /// One day after the last purchase is well inside the window: not yet.
    #[test]
    fn item_inside_the_window_is_not_suggested_yet() {
        let today = date(2026, 6, 24);
        let ev = evidence(
            "milk",
            &[date(2026, 6, 9), date(2026, 6, 16), date(2026, 6, 23)],
        );
        assert!(compute_suggestions(&ev, &[], &HashSet::new(), today).is_empty());
    }

    /// An item already on the list is never suggested.
    #[test]
    fn item_already_on_the_list_is_excluded() {
        let today = date(2026, 6, 30);
        let ev = evidence(
            "milk",
            &[date(2026, 6, 9), date(2026, 6, 16), date(2026, 6, 23)],
        );
        let excluded: HashSet<String> = ["milk".to_string()].into_iter().collect();
        assert!(compute_suggestions(&ev, &[], &excluded, today).is_empty());
    }

    /// A dismissal suppresses the item for one typical interval, then it returns.
    #[test]
    fn dismissal_pushes_the_next_suggestion_later() {
        let ev = evidence(
            "milk",
            &[date(2026, 6, 9), date(2026, 6, 16), date(2026, 6, 23)],
        );
        // Dismissed on the due day (interval 7): suppressed immediately after ...
        let dismissed = Dismissal {
            canonical_item_id: "milk".to_string(),
            day: date(2026, 6, 30),
        };
        assert!(compute_suggestions(
            &ev,
            std::slice::from_ref(&dismissed),
            &HashSet::new(),
            date(2026, 7, 5)
        )
        .is_empty());
        // ... and reappears once a full interval has elapsed since the dismissal.
        let out = compute_suggestions(&ev, &[dismissed], &HashSet::new(), date(2026, 7, 7));
        assert_eq!(out.len(), 1);
    }

    /// Most-overdue items rank first.
    #[test]
    fn results_are_ordered_most_overdue_first() {
        let today = date(2026, 6, 30);
        let mut ev = evidence(
            "weekly",
            &[date(2026, 6, 9), date(2026, 6, 16), date(2026, 6, 23)],
        );
        ev.extend(evidence(
            "fortnightly",
            &[date(2026, 5, 5), date(2026, 5, 19), date(2026, 6, 2)],
        ));
        let out = compute_suggestions(&ev, &[], &HashSet::new(), today);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].canonical_item_id, "fortnightly");
        assert_eq!(out[1].canonical_item_id, "weekly");
    }

    #[test]
    fn is_due_boundary_is_eighty_percent() {
        assert!(!is_due(7, 10)); // 70%
        assert!(is_due(8, 10)); // 80%
        assert!(is_due(30, 10)); // overdue
    }
}
