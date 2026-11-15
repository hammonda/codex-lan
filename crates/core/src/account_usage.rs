use crate::protocol::{AccountUsage, RateLimitBucket, RateLimitWindow};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Default)]
pub(crate) struct UsageCache {
    snapshot: AccountUsage,
    epoch: u64,
    reading: bool,
    updates: Vec<RateLimitBucket>,
}

impl UsageCache {
    pub(crate) fn begin_read(&mut self) -> u64 {
        self.reading = true;
        self.updates.clear();
        self.epoch
    }

    pub(crate) fn finish_read(&mut self, epoch: u64, response: Option<&Value>) -> AccountUsage {
        self.reading = false;
        if epoch == self.epoch {
            self.snapshot = response
                .map(parse_snapshot)
                .unwrap_or_else(|| AccountUsage {
                    unavailable_reason: Some("Usage is unavailable for this Codex account.".into()),
                    ..Default::default()
                });
            // Notifications emitted during a read are newer than its snapshot.
            for update in std::mem::take(&mut self.updates) {
                merge_bucket(&mut self.snapshot, update);
            }
        }
        self.updates.clear();
        self.snapshot.clone()
    }

    pub(crate) fn update(&mut self, params: &Value) -> Option<AccountUsage> {
        let bucket = parse_bucket(params.get("rateLimits")?, "codex")?;
        if self.reading {
            // Coalesce rolling updates by bucket rather than retaining an event log.
            let mut pending = AccountUsage {
                limits: std::mem::take(&mut self.updates),
                ..Default::default()
            };
            merge_bucket(&mut pending, bucket.clone());
            self.updates = pending.limits;
        }
        merge_bucket(&mut self.snapshot, bucket);
        Some(self.snapshot.clone())
    }

    pub(crate) fn account_changed(&mut self) -> AccountUsage {
        self.epoch = self.epoch.wrapping_add(1);
        self.updates.clear();
        self.snapshot = AccountUsage {
            unavailable_reason: Some("Account changed; refreshing usage.".into()),
            ..Default::default()
        };
        self.snapshot.clone()
    }
}

fn parse_snapshot(value: &Value) -> AccountUsage {
    let mut limits = BTreeMap::new();
    if let Some(bucket) = parse_bucket(&value["rateLimits"], "codex") {
        limits.insert(bucket.id.clone(), bucket);
    }
    if let Some(buckets) = value["rateLimitsByLimitId"].as_object() {
        for (id, value) in buckets {
            if let Some(bucket) = parse_bucket(value, id) {
                limits.insert(bucket.id.clone(), bucket);
            }
        }
    }
    AccountUsage {
        unavailable_reason: limits
            .is_empty()
            .then(|| "Codex has not reported usage limits.".into()),
        limits: limits.into_values().collect(),
        ordinary_usage_allowed: value["ordinaryUsageAllowed"].as_bool(),
    }
}

fn parse_bucket(value: &Value, fallback_id: &str) -> Option<RateLimitBucket> {
    let value = value.as_object()?;
    let string = |name| value.get(name).and_then(Value::as_str).map(str::to_owned);
    let primary = value.get("primary").and_then(parse_window);
    let secondary = value.get("secondary").and_then(parse_window);
    if primary.is_none() && secondary.is_none() && !value.contains_key("spendControlReached") {
        return None;
    }
    Some(RateLimitBucket {
        id: string("limitId").unwrap_or_else(|| fallback_id.into()),
        name: string("limitName"),
        model: string("normalModelSlug"),
        plan: string("planType"),
        primary,
        secondary,
        spend_control_reached: value.get("spendControlReached").and_then(Value::as_bool),
    })
}

fn parse_window(value: &Value) -> Option<RateLimitWindow> {
    Some(RateLimitWindow {
        used_percent: value["usedPercent"].as_i64()?.try_into().ok()?,
        window_minutes: value["windowDurationMins"].as_i64().filter(|n| *n > 0),
        resets_at: value["resetsAt"].as_i64().filter(|n| *n >= 0),
    })
}

fn merge_window(target: &mut Option<RateLimitWindow>, update: Option<RateLimitWindow>) {
    if let Some(mut update) = update {
        if let Some(previous) = target {
            update.window_minutes = update.window_minutes.or(previous.window_minutes);
            update.resets_at = update.resets_at.or(previous.resets_at);
        }
        *target = Some(update);
    }
}

fn merge_bucket(snapshot: &mut AccountUsage, update: RateLimitBucket) {
    snapshot.unavailable_reason = None;
    if let Some(previous) = snapshot
        .limits
        .iter_mut()
        .find(|limit| limit.id == update.id)
    {
        previous.name = update.name.or(previous.name.take());
        previous.model = update.model.or(previous.model.take());
        previous.plan = update.plan.or(previous.plan.take());
        merge_window(&mut previous.primary, update.primary);
        merge_window(&mut previous.secondary, update.secondary);
        // Native schema explicitly treats null as unavailable for this field.
        previous.spend_control_reached = update.spend_control_reached;
    } else {
        snapshot.limits.push(update);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sparse_updates_keep_windows_metadata_and_omit_private_fields() {
        let mut cache = UsageCache::default();
        let epoch = cache.begin_read();
        let initial = json!({"accountId":"private-id","ordinaryUsageAllowed":false,"rateLimits":{"limitId":"codex","planType":"plus","primary":{"usedPercent":10,"windowDurationMins":300,"resetsAt":1700000000},"secondary":{"usedPercent":20,"windowDurationMins":10080,"resetsAt":1700100000},"credits":{"balance":"private-credit-balance"}},"rateLimitResetCredits":{"credits":[{"id":"private-reset"}]}});
        // A sparse update arriving during the initial read must win, while the
        // completed read supplies the missing duration/plan and permission.
        cache.update(&json!({"rateLimits":{"limitId":"codex","secondary":{"usedPercent":30}}}));
        let usage = cache.finish_read(epoch, Some(&initial));
        assert_eq!(usage.ordinary_usage_allowed, Some(false));
        assert_eq!(usage.limits[0].secondary.as_ref().unwrap().used_percent, 30);
        assert_eq!(
            usage.limits[0].secondary.as_ref().unwrap().window_minutes,
            Some(10080)
        );
        assert_eq!(usage.limits[0].plan.as_deref(), Some("plus"));
        assert!(!serde_json::to_string(&usage).unwrap().contains("private"));
        let updated = cache.update(&json!({"rateLimits":{"primary":{"usedPercent":11},"secondary":null,"planType":null}})).unwrap();
        assert_eq!(
            updated.limits[0].primary.as_ref().unwrap().window_minutes,
            Some(300)
        );
        assert_eq!(updated.limits[0].secondary, usage.limits[0].secondary);
        assert_eq!(updated.ordinary_usage_allowed, Some(false));
    }

    #[test]
    fn full_refresh_and_account_changes_do_not_reuse_old_allowances() {
        let mut cache = UsageCache::default();
        let epoch = cache.begin_read();
        cache.finish_read(
            epoch,
            Some(
                &json!({"rateLimits":{"secondary":{"usedPercent":22,"windowDurationMins":10080}}}),
            ),
        );
        let epoch = cache.begin_read();
        let empty = cache.finish_read(epoch, Some(&json!({"rateLimits":{}})));
        assert!(empty.limits.is_empty());
        assert!(empty.unavailable_reason.is_some());
        let epoch = cache.begin_read();
        cache.account_changed();
        let stale = cache.finish_read(
            epoch,
            Some(
                &json!({"rateLimits":{"secondary":{"usedPercent":99,"windowDurationMins":10080}}}),
            ),
        );
        assert!(stale.limits.is_empty());
        let epoch = cache.begin_read();
        let missing = cache.finish_read(epoch, None);
        assert!(missing.limits.is_empty());
        assert!(missing.unavailable_reason.is_some());
        assert!(cache.update(&json!({"detail":"not-usage"})).is_none());
    }
}
