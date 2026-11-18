//! What tokens cost at Anthropic's API prices (the owner, 2026-11-17, M13: "split input and output
//! tokens so we can caluclate the costs of tokens in API prices"). Built in and updated with
//! releases (D-197); a model not listed has no price ("price unknown"), never a guess.
//!
//! Prices in dollars per million tokens, first-party API, as of 2026-11. Cache writes cost 1.25×
//! the input price when kept for 5 minutes and 2× when kept for an hour; cache reads differ by
//! model. Fast mode doubles every price.

/// A model's prices, dollars per million tokens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub write_5m: f64,
    pub write_1h: f64,
}

const fn p(input: f64, output: f64, cache_read: f64) -> Price {
    Price { input, output, cache_read, write_5m: input * 1.25, write_1h: input * 2.0 }
}

/// Model ids and their prices. Longer ids first: "claude-opus-5" must not catch "claude-opus-5-5".
const PRICES: &[(&str, Price)] = &[
    ("claude-fable-5-2", p(8.0, 40.0, 0.20)),
    ("claude-mythos-5-2", p(8.0, 40.0, 0.20)),
    ("claude-fable-5-1", p(10.0, 50.0, 0.25)),
    ("claude-mythos-5-1", p(10.0, 50.0, 0.25)),
    ("claude-fable-5", p(10.0, 50.0, 1.00)),
    ("claude-mythos-5", p(10.0, 50.0, 1.00)),
    ("claude-opus-5-5", p(4.0, 20.0, 0.20)),
    ("claude-opus-5", p(5.0, 25.0, 0.50)),
    ("claude-opus-4-8", p(5.0, 25.0, 0.50)),
    ("claude-opus-4-7", p(5.0, 25.0, 0.50)),
    ("claude-opus-4-6", p(5.0, 25.0, 0.50)),
    ("claude-sonnet-5-5", p(2.0, 10.0, 0.10)),
    ("claude-sonnet-5", p(2.0, 10.0, 0.20)),
    ("claude-sonnet-4-6", p(3.0, 15.0, 0.30)),
    ("claude-haiku-5-5", p(0.10, 0.50, 0.01)),
    ("claude-haiku-4-5", p(1.0, 5.0, 0.10)),
];

/// The prices of a model id, also with a date or a context suffix ("claude-opus-4-8-20260101",
/// "claude-opus-5-5[1m]").
pub fn of(model: &str) -> Option<Price> {
    let m = model.trim();
    let m = m.split('[').next().unwrap_or(m);
    PRICES
        .iter()
        .filter(|(id, _)| m == *id || m.strip_prefix(id).is_some_and(|rest| rest.starts_with('-') && rest[1..].bytes().all(|b| b.is_ascii_digit())))
        .max_by_key(|(id, _)| id.len())
        .map(|(_, p)| *p)
}

/// Token counts of one call or many of one model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    /// All cache writes, and of them those kept for an hour (the rest for 5 minutes).
    pub cache_write: i64,
    pub cache_write_1h: i64,
}

impl Tokens {
    pub fn total(&self) -> i64 {
        self.input + self.output + self.cache_read + self.cache_write
    }

    /// New tokens: everything but cache reads.
    pub fn new_tokens(&self) -> i64 {
        self.input + self.output + self.cache_write
    }
}

/// What these tokens of `model` cost in dollars; None for a model without a price. No tokens cost
/// nothing, whatever the model (Claude Code's own "<synthetic>" turns).
pub fn cost(model: &str, t: &Tokens, fast: bool) -> Option<f64> {
    if t.total() == 0 {
        return Some(0.0);
    }
    let p = of(model)?;
    let w1 = t.cache_write_1h.clamp(0, t.cache_write);
    let dollars = (t.input as f64 * p.input
        + t.output as f64 * p.output
        + t.cache_read as f64 * p.cache_read
        + (t.cache_write - w1) as f64 * p.write_5m
        + w1 as f64 * p.write_1h)
        / 1e6;
    Some(if fast { dollars * 2.0 } else { dollars })
}

/// `price_micro(model, input, output, cache_read, cache_write, cache_write_1h, fast)` in SQL: a
/// call's cost in millionths of a dollar, NULL for a model without a price. Cache writes whose
/// duration isn't known (`cache_write_1h` NULL) count at the hour's price.
pub fn register(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    use rusqlite::functions::FunctionFlags;
    conn.create_scalar_function(
        "price_micro",
        7,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let model: String = ctx.get(0)?;
            let cache_write: i64 = ctx.get(4)?;
            let t = Tokens {
                input: ctx.get(1)?,
                output: ctx.get(2)?,
                cache_read: ctx.get(3)?,
                cache_write,
                cache_write_1h: ctx.get::<Option<i64>>(5)?.unwrap_or(cache_write),
            };
            let fast: i64 = ctx.get(6)?;
            Ok(cost(&model, &t, fast != 0).map(|d| (d * 1e6).round() as i64))
        },
    )
}

/// The SQL for a row's cost, for `sum(…)`.
pub const SQL_COST: &str = "price_micro(model, input, output, cache_read, cache_write, cache_write_1h, fast)";

/// "$1,234", "$12.40", "$0.42": cents only below $100.
pub fn dollars(v: f64) -> String {
    if v >= 100.0 {
        let n = v.round() as i64;
        let s = n.to_string();
        let mut out = String::new();
        for (i, ch) in s.chars().enumerate() {
            if i > 0 && (s.len() - i).is_multiple_of(3) {
                out.push(',');
            }
            out.push(ch);
        }
        format!("${out}")
    } else {
        format!("${v:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_are_priced_by_their_own_id_and_unknown_ones_not_at_all() {
        assert_eq!(of("claude-opus-5-5").unwrap().input, 4.0);
        assert_eq!(of("claude-opus-5").unwrap().input, 5.0, "not Opus 5.5's");
        assert_eq!(of("claude-opus-4-8-20260101").unwrap().output, 25.0);
        assert_eq!(of("claude-opus-5-5[1m]").unwrap().cache_read, 0.20);
        assert_eq!(of("claude-opus-5-5x"), None);
        assert_eq!(of("gpt-9"), None);
        let t = Tokens { input: 1_000_000, output: 1_000_000, cache_read: 10_000_000, cache_write: 2_000_000, cache_write_1h: 1_000_000 };
        // 4 + 20 + 2 + 5 (5 min) + 8 (1 h)
        assert_eq!(cost("claude-opus-5-5", &t, false), Some(39.0));
        assert_eq!(cost("claude-opus-5-5", &t, true), Some(78.0), "fast mode doubles it");
        assert_eq!(cost("mystery", &t, false), None);
        assert_eq!(cost("<synthetic>", &Tokens::default(), false), Some(0.0));
        assert_eq!(dollars(1234.4), "$1,234");
        assert_eq!(dollars(12.4), "$12.40");
    }
}
