//! The cost an endpoint reports it billed (after OpenCode's
//! `reportedBilling`).

use serde_json::Value;

/// A billed amount in USD: a finite, non-negative number. Anything else is
/// malformed and reads as absent, without failing the stream.
fn amount(v: &Value) -> Option<f64> {
    v.as_f64().filter(|c| c.is_finite() && *c >= 0.0)
}

/// The USD an endpoint billed for a response: `cost` beside the token counts
/// (OpenRouter, LiteLLM, OpenCode Zen), else GitHub Copilot's
/// `copilot_usage.total_nano_aiu` on one of the frames that carry the usage
/// (billionths of an AI credit, which costs $0.01).
pub(crate) fn billed(usage: &Value, frames: &[&Value]) -> Option<f64> {
    amount(&usage["cost"]).or_else(|| {
        frames
            .iter()
            .find_map(|f| amount(&f["copilot_usage"]["total_nano_aiu"]))
            .map(|n| n / 100_000_000_000.0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reported_and_copilot_billing() {
        assert_eq!(billed(&json!({"cost": 0.25}), &[]), Some(0.25));
        assert_eq!(billed(&json!({"cost": 0}), &[]), Some(0.0));
        assert_eq!(billed(&json!({"cost": -1}), &[]), None);
        assert_eq!(billed(&json!({"cost": "1"}), &[]), None);
        let frame = json!({"copilot_usage": {"total_nano_aiu": 4_473_525_000u64}});
        assert_eq!(billed(&json!({}), &[&frame]), Some(0.04473525));
        // A reported cost wins over Copilot's.
        assert_eq!(billed(&json!({"cost": 1.5}), &[&frame]), Some(1.5));
    }
}
