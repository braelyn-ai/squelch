//! RECENCY: the one time-decay curve every ranked search leg blends in.
//!
//! Mail is not a document corpus. The thread a reader is looking for is
//! overwhelmingly the one that moved recently, and a pure relevance score will
//! happily rank a three-year-old newsletter above last Tuesday's contract
//! because the newsletter says the word more often. So every leg that RANKS
//! (keyword, semantic, hybrid) scales its score by how fresh the mail is. The
//! filter-only listing does not: it is already newest-first, which is this same
//! idea taken to its limit.
//!
//! ONE CURVE, TWO EVALUATORS. The keyword leg ranks in SQL, because it
//! paginates with LIMIT/OFFSET and must keep doing that EXACTLY; the recall
//! legs fuse their candidate lists in Rust. The curve therefore exists twice —
//! as [`boost`] and as the SQL text [`boost_sql`] builds — but both are
//! generated from the constants below, and a store test evaluates them side by
//! side across the whole age range so the pair cannot drift.
//!
//! SHAPE — hyperbolic, not exponential:
//!
//! ```text
//! boost(age_days) = FLOOR + (1 - FLOOR) / (1 + max(age_days, 0) / HALF_LIFE_DAYS)
//! ```
//!
//! - It starts at `1.0` for mail that just landed and decays toward [`FLOOR`],
//!   never to zero. Old mail is demoted, never buried: a term that only ever
//!   appeared in 2023 is still findable, it just stops outranking this week.
//! - `HALF_LIFE_DAYS` is where the decaying half has fallen by half, so the
//!   constant reads the way you would say it out loud.
//! - The tail is FAT (`1/x`, not `0.5^x`). The difference between one month old
//!   and two is worth arguing about; the difference between three years and
//!   four is not, and an exponential keeps insisting that it is.
//! - It needs nothing but `+`, `*` and `/`, so the SQL half does not depend on
//!   SQLite being built with `SQLITE_ENABLE_MATH_FUNCTIONS` (`pow`/`exp` live
//!   behind that flag and are not guaranteed present).
//!
//! AGE IS CLAMPED AT ZERO, and that is not defensive tidiness. `received_at`
//! comes from a `Date:` header an untrusted sender wrote. Un-clamped, mail
//! dated exactly `HALF_LIFE_DAYS` in the future divides by zero, and mail dated
//! further out scores NEGATIVE and sinks below every honest result — a header
//! nobody validates would be steering the ranking. Clamped, the best a
//! future-dated `Date:` can buy is "as good as new", which is where the
//! ordinary front of the list already is.

use chrono::{DateTime, Utc};

/// The age at which the decaying half of the boost has fallen by half.
///
/// 30 days: mail from this month is current, last month's is fading, last
/// quarter's is archive. That is the rhythm of a mailbox, not of a library.
pub const HALF_LIFE_DAYS: f64 = 30.0;

/// What the OLDEST mail keeps — the value the curve decays toward and never
/// reaches.
///
/// This is the whole tuning knob, because it is exactly how far recency is
/// allowed to tilt a ranking: at most `1 / FLOOR` (~2.9x) of relevance score.
/// A clearly better old match still wins; a marginally better old match no
/// longer does.
pub const FLOOR: f64 = 0.35;

const SECS_PER_DAY: f64 = 86_400.0;

/// The recency factor for a message received at `received_at`, as of `now`.
/// Always within `[FLOOR, 1.0]`.
pub fn boost(received_at: DateTime<Utc>, now: DateTime<Utc>) -> f64 {
    curve((now - received_at).num_seconds() as f64 / SECS_PER_DAY)
}

/// [`boost`] over an age already in days. Split out so the curve can be walked
/// without inventing timestamps for every point on it.
pub fn curve(age_days: f64) -> f64 {
    // NaN would survive `max` on the wrong side and poison a sort comparator;
    // an unreadable age is treated as brand new, matching the clamp's logic
    // that no header value should be able to push a row DOWN either.
    let age = if age_days.is_nan() {
        0.0
    } else {
        age_days.max(0.0)
    };
    FLOOR + (1.0 - FLOOR) / (1.0 + age / HALF_LIFE_DAYS)
}

/// [`curve`] as a SQLite expression, for the leg that ranks in SQL.
///
/// `received_col` is the qualified `received_at` column; `now_param` is the
/// PLACEHOLDER holding the query time as RFC3339 text. The clock is BOUND
/// rather than read from SQLite's own `julianday('now')`, so every leg of one
/// search dates from the same instant and a test can seed relative to it.
///
/// `julianday()` returns NULL on text it cannot parse and NULL propagates
/// through `max()`, so a row with an unreadable timestamp COALESCEs to
/// [`FLOOR`]: it ranks as ancient rather than dropping out of the results or
/// sorting as NULL.
pub(crate) fn boost_sql(received_col: &str, now_param: &str) -> String {
    // `{:?}` on an f64 always prints a decimal point and round-trips exactly,
    // so SQLite parses back the same value Rust computed with — which is what
    // lets the drift test compare the two evaluators for equality.
    format!(
        "COALESCE({floor:?} + {decaying:?} / \
         (1.0 + max(0.0, julianday({now_param}) - julianday({received_col})) / {half_life:?}), \
         {floor:?})",
        floor = FLOOR,
        decaying = 1.0 - FLOOR,
        half_life = HALF_LIFE_DAYS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn fresh_mail_keeps_all_of_its_relevance() {
        assert_eq!(curve(0.0), 1.0);
    }

    #[test]
    fn the_half_life_halves_the_decaying_half() {
        // Not the whole score — the part above the floor.
        let expected = FLOOR + (1.0 - FLOOR) / 2.0;
        assert!((curve(HALF_LIFE_DAYS) - expected).abs() < 1e-12);
    }

    #[test]
    fn the_curve_falls_toward_the_floor_and_never_through_it() {
        let mut prev = curve(0.0);
        for days in [1.0, 7.0, 30.0, 90.0, 365.0, 3650.0, 36500.0] {
            let b = curve(days);
            assert!(
                b < prev,
                "curve must fall: {days} days gave {b} after {prev}"
            );
            assert!(b > FLOOR, "curve must stay above the floor: {days} => {b}");
            prev = b;
        }
        // Ten years in it is still within a whisker of the floor, but above it.
        assert!(curve(3650.0) < FLOOR + 0.01);
    }

    #[test]
    fn a_future_date_header_buys_nothing_beyond_brand_new() {
        // The clamp: without it, -HALF_LIFE_DAYS divides by zero and anything
        // past that scores negative.
        for days in [-1.0, -HALF_LIFE_DAYS, -3650.0] {
            assert_eq!(curve(days), 1.0, "future age {days} must clamp to new");
        }
        assert_eq!(
            curve(f64::NAN),
            1.0,
            "an unreadable age must not poison a sort"
        );
    }

    #[test]
    fn boost_reads_the_gap_between_two_instants() {
        let now = Utc::now();
        let a_month_ago = now - Duration::days(HALF_LIFE_DAYS as i64);
        assert!((boost(now, now) - 1.0).abs() < 1e-9);
        assert!((boost(a_month_ago, now) - curve(HALF_LIFE_DAYS)).abs() < 1e-6);
    }

    #[test]
    fn the_sql_expression_names_the_same_constants() {
        let sql = boost_sql("m.received_at", "?7");
        assert!(
            sql.contains("julianday(?7)"),
            "the clock is a bound parameter"
        );
        assert!(sql.contains("julianday(m.received_at)"));
        assert!(sql.contains("max(0.0,"), "the future clamp travels with it");
        assert!(sql.contains("0.35"), "the floor is the one in FLOOR");
        assert!(
            sql.contains("30.0"),
            "the half-life is the one in HALF_LIFE_DAYS"
        );
    }
}
