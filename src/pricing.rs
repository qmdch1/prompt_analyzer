//! What a prompt would cost at API list prices, and the USD→KRW rate for showing it in won.
use serde_json::{json, Value};
use std::{env, sync::Mutex, time::{Duration, Instant}};

/// (model id prefix, input, cached input, output) in USD per 1M tokens — most specific prefix first.
/// Sources: Anthropic model table (2026-06) and developers.openai.com/api/docs/pricing (2026-09).
/// Cache writes are billed like fresh input here, so Claude costs are a slight underestimate.
const PRICES: &[(&str, f64, f64, f64)] = &[
    ("claude-fable-5-1", 10.0, 0.25, 50.0),
    ("claude-fable-5", 10.0, 1.0, 50.0),
    ("claude-mythos-5", 10.0, 1.0, 50.0),
    ("claude-opus-5-5", 4.0, 0.20, 20.0),
    ("claude-opus-5", 5.0, 0.50, 25.0),
    ("claude-opus-4-8", 5.0, 0.50, 25.0),
    ("claude-opus-4-7", 5.0, 0.50, 25.0),
    ("claude-opus-4-6", 5.0, 0.50, 25.0),
    ("claude-sonnet-5", 2.0, 0.20, 10.0),
    ("claude-sonnet-4", 3.0, 0.30, 15.0),
    ("claude-haiku-4-5", 1.0, 0.10, 5.0),
    ("gpt-6-astra", 10.0, 1.0, 50.0),
    ("gpt-6-sol", 2.0, 0.20, 10.0),
    ("gpt-6-luna", 0.10, 0.01, 0.50),
    ("gpt-5.6-sol", 4.0, 0.40, 20.0),
    ("gpt-5.6-terra", 2.0, 0.20, 12.0),
    ("gpt-5.6-luna", 0.20, 0.02, 1.20),
    ("gpt-5.5-pro", 30.0, 30.0, 180.0),
    ("gpt-5.5", 5.0, 0.50, 30.0),
    ("gpt-5.3-codex", 1.75, 0.175, 14.0),
    ("gpt-5-mini", 0.25, 0.025, 2.0),
    ("gpt-5-nano", 0.05, 0.005, 0.40),
    ("gpt-5", 1.25, 0.125, 10.0),
];

/// `input` includes `cached`, as both agents report it. None for a model without a known price.
pub fn cost_usd(model: &str, input: i64, cached: i64, output: i64) -> Option<f64> {
    let (_, fresh_rate, cached_rate, output_rate) = PRICES.iter().find(|(prefix, ..)| model.starts_with(prefix))?;
    let fresh = (input - cached).max(0) as f64;
    Some((fresh * fresh_rate + cached as f64 * cached_rate + output as f64 * output_rate) / 1_000_000.0)
}

const FX_TTL: Duration = Duration::from_secs(3600);
/// Used when the rate can't be fetched; override with USD_KRW.
const FALLBACK_USD_KRW: f64 = 1355.0;

/// USD→KRW from the ECB reference rate (frankfurter.dev), cached for an hour.
pub async fn usd_krw(http: &reqwest::Client, cache: &Mutex<Option<(Instant, Value)>>) -> Value {
    if let Some((at, v)) = cache.lock().unwrap().as_ref() {
        if at.elapsed() < FX_TTL { return v.clone(); }
    }
    let fetched = async {
        let r: Value = http.get("https://api.frankfurter.dev/v1/latest?base=USD&symbols=KRW")
            .timeout(Duration::from_secs(5)).send().await.ok()?.json().await.ok()?;
        Some(json!({"usd_krw": r["rates"]["KRW"].as_f64()?, "date": r["date"], "source": "ECB (frankfurter.dev)"}))
    }.await;
    let v = fetched.unwrap_or_else(|| {
        let rate = env::var("USD_KRW").ok().and_then(|v| v.parse().ok()).unwrap_or(FALLBACK_USD_KRW);
        json!({"usd_krw": rate, "date": null, "source": "기본값"})
    });
    *cache.lock().unwrap() = Some((Instant::now(), v.clone()));
    v
}

#[cfg(test)]
mod tests {
    use super::cost_usd;

    #[test]
    fn prices_by_most_specific_model_prefix() {
        // Opus 5.5: 1M fresh input $4, 1M cached $0.20, 1M output $20.
        assert_eq!(cost_usd("claude-opus-5-5", 2_000_000, 1_000_000, 1_000_000), Some(24.2));
        assert_eq!(cost_usd("claude-haiku-4-5-20251001", 1_000_000, 0, 0), Some(1.0));
        assert_eq!(cost_usd("gpt-5.6-sol", 1_000_000, 0, 0), Some(4.0));  // not the gpt-5 row
        assert_eq!(cost_usd("gpt-6-astra", 16_804, 7_168, 7), Some((9_636.0 * 10.0 + 7_168.0 * 1.0 + 7.0 * 50.0) / 1e6));
        assert_eq!(cost_usd("claude", 0, 0, 0), None);
    }
}
