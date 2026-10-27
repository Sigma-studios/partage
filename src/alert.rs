//! Discord alerts about storage, tagging @everyone.

use crate::{App, html::fmt_bytes};
use serde_json::json;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

/// Usage levels (percent of the limit) that send an alert when crossed.
const LEVELS: [u64; 3] = [100, 90, 80];

/// At most one "upload blocked" alert per this long.
const BLOCKED_EVERY: Duration = Duration::from_secs(3600);

pub async fn send(app: &App, content: String) -> anyhow::Result<()> {
    let Some(url) = app.webhook() else {
        tracing::warn!("no Discord webhook set, alert not sent: {content}");
        return Ok(());
    };
    app.http
        .post(&url)
        .json(&json!({
            "content": content,
            "allowed_mentions": { "parse": ["everyone"] },
        }))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

/// Alerts once each time usage climbs past 80%, 90% or 100% of the limit,
/// and re-arms a level once usage drops back under it.
pub async fn check_usage(app: &App) {
    let _serial = app.alert_lock.lock().await;
    if app.webhook().is_none() {
        // Levels stay unsent until there is somewhere to send them.
        return;
    }
    let (used, limit) = (app.used(), app.limit());
    let percent = (used as u128 * 100 / limit.max(1) as u128) as u64;
    let level = LEVELS.into_iter().find(|l| percent >= *l).unwrap_or(0);
    let previous: u64 = match app.db.setting("alert_level") {
        Ok(v) => v.and_then(|v| v.parse().ok()).unwrap_or(0),
        Err(e) => return tracing::error!("reading alert level: {e:#}"),
    };
    if level > previous {
        let what = if level >= 100 {
            "is **full**: new uploads are blocked until files are deleted or the limit is raised"
        } else {
            "is filling up"
        };
        let msg = format!(
            "@everyone ⚠️ partage storage at **{percent}%** ({} of {}) {what}.",
            fmt_bytes(used),
            fmt_bytes(limit)
        );
        if let Err(e) = send(app, msg).await {
            // Not recorded as sent: the next check tries again.
            return tracing::error!("sending storage alert: {e:#}");
        }
    }
    if level != previous
        && let Err(e) = app.db.set_setting("alert_level", &level.to_string())
    {
        tracing::error!("saving alert level: {e:#}");
    }
}

/// An upload was refused for lack of space.
pub fn upload_blocked(app: Arc<App>) {
    {
        let mut last = app.last_blocked_alert.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < BLOCKED_EVERY) {
            return;
        }
        *last = Some(Instant::now());
    }
    tokio::spawn(async move {
        let msg = format!(
            "@everyone 🛑 partage **blocked an upload**: it would have gone over the storage \
             limit ({} of {} used).",
            fmt_bytes(app.used()),
            fmt_bytes(app.limit())
        );
        if let Err(e) = send(&app, msg).await {
            tracing::error!("sending blocked-upload alert: {e:#}");
        }
    });
}
