use serde_json::Value;

pub const PRICE_DATE: &str = "2026-11-15";
pub const PRICE_SOURCE: &str = "https://developers.openai.com/api/docs/pricing";

/// The native total is cumulative across the thread, not a per-event delta.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThreadUsage {
    totals: Option<TokenTotals>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
struct TokenTotals {
    input: Option<u64>,
    cached: Option<u64>,
    cache_write: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
    total: Option<u64>,
}

impl ThreadUsage {
    /// Structured view shared by GUI and MCP clients, with the same pricing assumptions.
    pub fn snapshot(&self, model: Option<&str>) -> Value {
        serde_json::json!({
            "tokens": self.totals,
            "api_equivalent_usd": self.estimate_usd(model),
            "price_date": PRICE_DATE,
            "price_source": PRICE_SOURCE,
            "assumptions": self.tooltip(model),
        })
    }

    /// Accepts the params of thread/tokenUsage/updated. Never sums snapshots.
    pub fn update(&mut self, params: &Value) {
        let Some(total) = params
            .pointer("/tokenUsage/total")
            .filter(|v| v.is_object())
        else {
            return;
        };
        self.totals = Some(TokenTotals {
            input: total["inputTokens"].as_u64(),
            cached: total["cachedInputTokens"].as_u64(),
            // The installed app-server schema defaults this optional field to 0.
            cache_write: match total.get("cacheWriteInputTokens") {
                None => Some(0),
                Some(value) => value.as_u64(),
            },
            output: total["outputTokens"].as_u64(),
            reasoning: total["reasoningOutputTokens"].as_u64(),
            total: total["totalTokens"].as_u64(),
        });
    }

    pub fn estimate_usd(&self, model: Option<&str>) -> Option<f64> {
        let totals = self.totals.as_ref()?;
        let price = price(model?)?;
        let input = totals.input?;
        let cached = totals.cached?;
        let writes = totals.cache_write?;
        let output = totals.output?;
        // Cached reads and cache writes are subsets of input. Invalid snapshots
        // must not produce a plausible-looking price through saturating math.
        // https://developers.openai.com/api/docs/guides/prompt-caching specifies
        // that cache-write pricing replaces ordinary input pricing, not adds to it.
        let uncached = input.checked_sub(cached.checked_add(writes)?)?;
        let write_cost = if writes == 0 {
            0.0
        } else {
            writes as f64 * price.cache_write?
        };
        // Reasoning is already included in outputTokens.
        Some(
            (uncached as f64 * price.input
                + cached as f64 * price.cached
                + write_cost
                + output as f64 * price.output)
                / 1_000_000.0,
        )
    }

    pub fn label(&self, model: Option<&str>) -> String {
        let input = self.totals.as_ref().and_then(|t| t.input);
        let output = self.totals.as_ref().and_then(|t| t.output);
        let cost = match self.estimate_usd(model) {
            Some(cost) if cost > 0.0 && cost < 0.0001 => "~<$0.0001".into(),
            Some(cost) => format!("~${cost:.4}"),
            None => "—".into(),
        };
        format!(
            "in {} · out {} · API {cost}",
            compact_count(input),
            compact_count(output)
        )
    }

    pub fn tooltip(&self, model: Option<&str>) -> String {
        let mut text = String::new();
        if let Some(totals) = &self.totals {
            text.push_str(&format!(
                "Thread tokens\nInput: {} (cached: {}, cache writes: {})\nOutput: {} (reasoning: {}, included in output)\nTotal: {}\n\n",
                exact_count(totals.input), exact_count(totals.cached), exact_count(totals.cache_write),
                exact_count(totals.output), exact_count(totals.reasoning), exact_count(totals.total),
            ));
        } else {
            text.push_str("Waiting for native thread token counts.\n\n");
        }
        if let Some(price) = model.and_then(price) {
            text.push_str(&format!(
                "API-equivalent estimate for {}\nUSD per 1M tokens: input ${}, cached ${}, output ${}.",
                model.unwrap_or_default(), price.input, price.cached, price.output,
            ));
            if let Some(write) = price.cache_write {
                text.push_str(&format!(" Cache writes ${write} (30-minute retention)."));
            }
            if self.totals.is_some() && self.estimate_usd(model).is_none() {
                text.push_str("\nPrice unavailable: token breakdown is missing or inconsistent, or its cache-write rate is unpublished.");
            }
            text.push_str("\nStandard short-context token rates; excludes long-context, service-tier, regional and tool charges. Mixed-model usage and other cache-retention settings can differ. Not a Codex subscription charge.");
        } else {
            text.push_str("API estimate unavailable: no verified published price for this model. Token counts remain available.");
        }
        text.push_str(&format!(
            "\nPrices checked {PRICE_DATE}; bundled snapshot.\n{PRICE_SOURCE}"
        ));
        text
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Price {
    input: f64,
    cached: f64,
    cache_write: Option<f64>,
    output: f64,
}

fn price(model: &str) -> Option<Price> {
    // Verified against the official pricing page and each model's pricing page
    // on PRICE_DATE. Match published IDs exactly: a shared prefix does not
    // establish pricing for a preview, provider alias or unlisted snapshot.
    let (input, cached, cache_write, output) = match model {
        "gpt-6-astra" => (10.0, 1.0, Some(12.5), 50.0),
        "gpt-6.1-sol" => (2.0, 0.1, Some(2.5), 10.0),
        "gpt-6.1-terra" => (1.0, 0.05, Some(1.25), 5.0),
        "gpt-6-sol" => (2.0, 0.2, Some(2.5), 10.0),
        "gpt-6-luna" => (0.1, 0.01, Some(0.125), 0.5),
        "gpt-5.3-codex" => (1.75, 0.175, None, 14.0),
        _ => return None,
    };
    Some(Price {
        input,
        cached,
        cache_write,
        output,
    })
}

fn exact_count(count: Option<u64>) -> String {
    count
        .map(|n| n.to_string())
        .unwrap_or_else(|| "unavailable".into())
}

fn compact_count(count: Option<u64>) -> String {
    match count {
        None => "—".into(),
        Some(n) if n >= 1_000_000_000 => format!("{:.1}B", n as f64 / 1_000_000_000.0),
        Some(n) if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1_000_000.0),
        Some(n) if n >= 1_000 => format!("{:.1}k", n as f64 / 1_000.0),
        Some(n) => n.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn usage(total: Value) -> ThreadUsage {
        let mut usage = ThreadUsage::default();
        usage.update(&json!({"tokenUsage":{"total":total}}));
        usage
    }

    #[test]
    fn snapshots_replace_totals_without_summing_last_or_reasoning() {
        let event = json!({"tokenUsage":{
            "total":{"inputTokens":1_000_000,"cachedInputTokens":500_000,"outputTokens":100_000,"reasoningOutputTokens":80_000,"totalTokens":1_100_000},
            "last":{"inputTokens":100,"outputTokens":10}
        }});
        let mut usage = ThreadUsage::default();
        usage.update(&event);
        let once = usage.clone();
        usage.update(&event);
        assert_eq!(usage, once);
        assert_eq!(usage.estimate_usd(Some("gpt-6.1-sol")), Some(2.05));
        assert_eq!(
            usage.label(Some("gpt-6.1-sol")),
            "in 1.0M · out 100.0k · API ~$2.0500"
        );
        usage.update(&json!({"tokenUsage":{"total":{"inputTokens":100,"cachedInputTokens":0,"outputTokens":10}}}));
        assert_eq!(usage.totals.as_ref().unwrap().input, Some(100));
        assert_eq!(usage.estimate_usd(Some("gpt-6.1-sol")), Some(0.0003));
    }

    #[test]
    fn cache_reads_and_writes_replace_the_regular_input_rate() {
        let usage = usage(
            json!({"inputTokens":1_000_000,"cachedInputTokens":400_000,"cacheWriteInputTokens":100_000,"outputTokens":100_000}),
        );
        assert_eq!(usage.estimate_usd(Some("gpt-6.1-sol")), Some(2.29));
        assert_eq!(usage.estimate_usd(Some("gpt-5.3-codex")), None);
        assert!(
            usage
                .tooltip(Some("gpt-6.1-sol"))
                .contains("30-minute retention")
        );
    }

    #[test]
    fn missing_or_invalid_counts_do_not_invent_usage_or_prices() {
        assert_eq!(
            ThreadUsage::default().label(Some("gpt-6-sol")),
            "in — · out — · API —"
        );
        for total in [
            json!({"inputTokens":100,"outputTokens":20}),
            json!({"inputTokens":-1,"cachedInputTokens":0,"outputTokens":20}),
            json!({"inputTokens":100,"cachedInputTokens":101,"outputTokens":20}),
            json!({"inputTokens":100,"cachedInputTokens":80,"cacheWriteInputTokens":30,"outputTokens":20}),
            json!({"inputTokens":100,"cachedInputTokens":0,"cacheWriteInputTokens":null,"outputTokens":20}),
        ] {
            assert_eq!(usage(total).estimate_usd(Some("gpt-6-sol")), None);
        }
        let mut usage = usage(json!({"inputTokens":100,"cachedInputTokens":0,"outputTokens":20}));
        let before = usage.clone();
        usage.update(&json!({"tokenUsage":{"last":{"inputTokens":9999}}}));
        assert_eq!(usage, before);
    }

    #[test]
    fn unknown_models_and_unverified_suffixes_never_inherit_prices() {
        let usage = usage(json!({"inputTokens":100,"cachedInputTokens":0,"outputTokens":20}));
        for model in [
            None,
            Some("gpt-unknown"),
            Some("gpt-6.1-sol-preview"),
            Some("gpt-6.1-sol-2026-11-15"),
            Some("openai/gpt-6-sol"),
        ] {
            assert_eq!(usage.estimate_usd(model), None);
            assert!(usage.label(model).contains("in 100 · out 20"));
            assert!(usage.tooltip(model).contains("no verified published price"));
        }
    }

    #[test]
    fn published_rates_are_distinct_and_tiny_estimates_remain_visible() {
        let million_tokens =
            usage(json!({"inputTokens":1_000_000,"cachedInputTokens":0,"outputTokens":1_000_000}));
        for (model, cost) in [
            ("gpt-6-astra", 60.0),
            ("gpt-6.1-sol", 12.0),
            ("gpt-6.1-terra", 6.0),
            ("gpt-6-sol", 12.0),
            ("gpt-6-luna", 0.6),
            ("gpt-5.3-codex", 15.75),
        ] {
            assert!((million_tokens.estimate_usd(Some(model)).unwrap() - cost).abs() < 1e-10);
        }
        let tiny = usage(json!({"inputTokens":1,"cachedInputTokens":0,"outputTokens":1}));
        assert!(tiny.label(Some("gpt-6-luna")).contains("~<$0.0001"));
        assert!(tiny.tooltip(Some("gpt-6-luna")).contains(PRICE_DATE));
        assert!(tiny.tooltip(Some("gpt-6-luna")).contains(PRICE_SOURCE));
    }
}
