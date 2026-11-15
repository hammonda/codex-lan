use crate::{AMBER, BORDER, MINT, MUTED, PANEL, RED, mono};
use codex_lan_core::protocol::{AccountUsage, RateLimitBucket, RateLimitWindow};
use eframe::egui::{self, Color32};
use std::time::{SystemTime, UNIX_EPOCH};

const WEEK_MINUTES: i64 = 7 * 24 * 60;

/// Account-wide allowance reported by the server's signed-in Codex process.
/// This deliberately does not infer a weekly limit from a window's position.
pub(crate) fn allowance_badge(ui: &mut egui::Ui, usage: Option<&AccountUsage>) {
    let (label, color) = badge_summary(usage);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    egui::Frame::new()
        .fill(PANEL)
        .stroke(egui::Stroke::new(0.8, BORDER))
        .corner_radius(3)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| ui.label(mono(label, 11.0, color)))
        .inner
        .on_hover_ui(|ui| {
            ui.set_max_width(420.0);
            ui.label(mono("Server account · weekly allowance", 11.0, MUTED));
            let Some(usage) = usage else {
                ui.label("Waiting for the server's Codex usage limits.");
                return;
            };
            if let Some(reason) = &usage.unavailable_reason {
                ui.label(reason);
            }
            let windows = weekly_windows(usage);
            if windows.is_empty() {
                if usage.unavailable_reason.is_none() {
                    ui.label("Codex did not report a weekly allowance for this account.");
                }
            } else {
                if usage.unavailable_reason.is_some() {
                    ui.label(mono("Last reported limits", 10.0, MUTED));
                } else if windows.len() > 1 {
                    ui.label("Separate limits; the badge shows their remaining range.");
                }
                for (bucket, window) in windows {
                    let name = bucket.name.as_deref().unwrap_or(&bucket.id);
                    let model = bucket.model.as_deref().filter(|model| *model != name);
                    let label = match model {
                        Some(model) => format!("{name} · {model}"),
                        None => name.to_owned(),
                    };
                    ui.label(mono(label, 11.0, MINT));
                    ui.label(format!(
                        "{}% left · {}% used · {}",
                        remaining(window),
                        window.used_percent,
                        reset_text(window.resets_at, now),
                    ));
                    if let Some(plan) = &bucket.plan {
                        ui.label(mono(format!("plan: {plan}"), 10.0, MUTED));
                    }
                    if bucket.spend_control_reached == Some(true) {
                        ui.colored_label(RED, "Spending control reached for this limit.");
                    }
                }
            }
            if usage.ordinary_usage_allowed == Some(false) {
                ui.colored_label(
                    RED,
                    "Ordinary Codex usage is currently blocked by an account limit.",
                );
            }
            ui.label(mono(
                "Shared across this account, not just this session.",
                10.0,
                MUTED,
            ));
        });
}

fn weekly_windows(usage: &AccountUsage) -> Vec<(&RateLimitBucket, &RateLimitWindow)> {
    usage
        .limits
        .iter()
        .flat_map(|bucket| {
            [bucket.primary.as_ref(), bucket.secondary.as_ref()]
                .into_iter()
                .flatten()
                .filter(|window| {
                    window.window_minutes == Some(WEEK_MINUTES) && window.used_percent >= 0
                })
                .map(move |window| (bucket, window))
        })
        .collect()
}

fn remaining(window: &RateLimitWindow) -> i32 {
    100 - window.used_percent.clamp(0, 100)
}

fn badge_summary(usage: Option<&AccountUsage>) -> (String, Color32) {
    let Some(usage) = usage else {
        return ("week —".into(), MUTED);
    };
    if usage.unavailable_reason.is_some() {
        return ("week —".into(), MUTED);
    }
    let windows = weekly_windows(usage);
    let Some(low) = windows.iter().map(|(_, window)| remaining(window)).min() else {
        return ("week —".into(), MUTED);
    };
    let high = windows
        .iter()
        .map(|(_, window)| remaining(window))
        .max()
        .unwrap_or(low);
    let blocked = usage.ordinary_usage_allowed == Some(false)
        || usage
            .limits
            .iter()
            .any(|bucket| bucket.spend_control_reached == Some(true));
    let color = if blocked || low <= 10 {
        RED
    } else if low <= 25 {
        AMBER
    } else {
        MINT
    };
    let range = if low == high {
        low.to_string()
    } else {
        format!("{low}–{high}")
    };
    let suffix = if blocked { " !" } else { "" };
    (format!("week {range}% left{suffix}"), color)
}

fn reset_text(resets_at: Option<i64>, now: u64) -> String {
    let Some(reset) = resets_at.and_then(|value| u64::try_from(value).ok()) else {
        return "reset time unavailable".into();
    };
    if reset <= now {
        return "reset due; awaiting an updated limit".into();
    }
    let minutes = (reset - now).div_ceil(60);
    let days = minutes / (24 * 60);
    let hours = minutes / 60 % 24;
    if days > 0 {
        format!("resets in {days}d {hours}h")
    } else if hours > 0 {
        format!("resets in {hours}h {}m", minutes % 60)
    } else {
        format!("resets in {minutes}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn usage(limits: serde_json::Value) -> AccountUsage {
        serde_json::from_value(json!({
            "limits": limits,
            "ordinary_usage_allowed": true,
            "unavailable_reason": null
        }))
        .unwrap()
    }

    fn bucket(
        id: &str,
        primary: serde_json::Value,
        secondary: serde_json::Value,
    ) -> serde_json::Value {
        json!({"id":id,"name":null,"model":null,"plan":null,"primary":primary,"secondary":secondary,"spend_control_reached":null})
    }

    fn window(used: i32, minutes: Option<i64>) -> serde_json::Value {
        json!({"used_percent":used,"window_minutes":minutes,"resets_at":null})
    }

    #[test]
    fn only_explicit_weekly_windows_count_and_separate_limits_remain_separate() {
        let account = usage(json!([
            bucket("short-only", window(99, Some(300)), window(20, None)),
            bucket(
                "codex",
                window(16, Some(WEEK_MINUTES)),
                serde_json::Value::Null
            ),
            bucket(
                "model",
                serde_json::Value::Null,
                window(62, Some(WEEK_MINUTES))
            )
        ]));
        assert_eq!(weekly_windows(&account).len(), 2);
        assert_eq!(badge_summary(Some(&account)).0, "week 38–84% left");
        let missing = usage(json!([bucket(
            "unknown",
            serde_json::Value::Null,
            window(0, None)
        )]));
        assert_eq!(badge_summary(Some(&missing)).0, "week —");
    }

    #[test]
    fn unavailable_and_blocked_limits_cannot_look_like_unused_allowance() {
        assert_eq!(badge_summary(None).0, "week —");
        let mut account = usage(json!([bucket(
            "codex",
            window(110, Some(WEEK_MINUTES)),
            serde_json::Value::Null
        )]));
        assert_eq!(badge_summary(Some(&account)), ("week 0% left".into(), RED));
        account.limits[0].primary.as_mut().unwrap().used_percent = 5;
        account.ordinary_usage_allowed = Some(false);
        assert_eq!(
            badge_summary(Some(&account)),
            ("week 95% left !".into(), RED)
        );
        account.unavailable_reason = Some("Request failed".into());
        assert_eq!(badge_summary(Some(&account)).0, "week —");
        account.unavailable_reason = None;
        account.limits[0].primary.as_mut().unwrap().used_percent = -1;
        assert_eq!(badge_summary(Some(&account)).0, "week —");
    }

    #[test]
    fn reset_countdowns_preserve_missing_and_expired_states() {
        assert_eq!(reset_text(None, 1000), "reset time unavailable");
        assert_eq!(reset_text(Some(-1), 1000), "reset time unavailable");
        assert_eq!(
            reset_text(Some(999), 1000),
            "reset due; awaiting an updated limit"
        );
        assert_eq!(reset_text(Some(1001), 1000), "resets in 1m");
        assert_eq!(reset_text(Some(1000 + 3660), 1000), "resets in 1h 1m");
        assert_eq!(
            reset_text(Some(1000 + 2 * 86400 + 10800), 1000),
            "resets in 2d 3h"
        );
    }
}
