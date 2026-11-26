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
///
/// Bifrost's unified API reports the cost an upstream billed as an object,
/// `{"input_cost", "output_cost", "total_cost", …}`: its `total_cost` is
/// the amount, and an object without one (or with zero, which Bifrost
/// leaves out) says nothing was reported.
pub(crate) fn billed(usage: &Value, frames: &[&Value]) -> Option<f64> {
    let cost = match &usage["cost"] {
        Value::Object(o) => o.get("total_cost").and_then(amount).filter(|c| *c > 0.0),
        other => amount(other),
    };
    cost.or_else(|| {
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
        // Bifrost's nested shape.
        assert_eq!(
            billed(
                &json!({"cost": {"input_cost": 0.1, "output_cost": 0.2, "total_cost": 0.3}}),
                &[]
            ),
            Some(0.3)
        );
        assert_eq!(billed(&json!({"cost": {"input_cost": 0.1}}), &[]), None);
        assert_eq!(billed(&json!({"cost": {"total_cost": 0}}), &[]), None);
        assert_eq!(billed(&json!({"cost": {"total_cost": -2}}), &[]), None);
        assert_eq!(billed(&json!({"cost": {"total_cost": "0.3"}}), &[]), None);
    }
}
