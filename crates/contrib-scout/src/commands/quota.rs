//! `quota`: shared quota snapshots and all-agent window usage (§9, §10).
//! Local only: reads the `quota` and `request_log` tables, zero requests.

use super::Context;
use crate::render::Report;
use anyhow::Result;
use chrono::TimeDelta;
use contrib_scout_core::coordinator::Resource;
use serde_json::json;

pub fn run(ctx: &mut Context<'_>) -> Result<Report> {
    let config = ctx.config()?;
    let store = ctx.open_store()?;
    let now = ctx.now;
    let minute_ago = now - TimeDelta::minutes(1);
    let hour_ago = now - TimeDelta::hours(1);
    let windows = config.windows();
    let reserves = config.reserves();
    let snapshots = store.quotas()?;

    let minute = store.requests_since(None, minute_ago)?;
    let hour = store.requests_since(None, hour_ago)?;
    let search_minute = store.requests_since(Some(Resource::Search), minute_ago)?;

    let mut resources = Vec::new();
    let mut human = vec![format!(
        "{:<8} {:>9} {:>7} {:>8} {:>6} {:>6}  {}",
        "RESOURCE", "REMAINING", "LIMIT", "RESERVE", "1MIN", "1H", "RESETS"
    )];
    for resource in Resource::ALL {
        let snapshot = snapshots.iter().find(|s| s.resource == resource);
        let used_minute = store.requests_since(Some(resource), minute_ago)?;
        let used_hour = store.requests_since(Some(resource), hour_ago)?;
        let reserve = reserves.get(resource);
        resources.push(json!({
            "resource": resource,
            "snapshot": snapshot,
            "reserve": reserve,
            "requests_last_minute": used_minute,
            "requests_last_hour": used_hour,
        }));
        let (remaining, limit, resets) = match snapshot {
            Some(s) => (
                s.remaining.to_string(),
                s.limit.to_string(),
                s.reset_at.to_rfc3339(),
            ),
            None => ("-".into(), "-".into(), "not observed yet".into()),
        };
        human.push(format!(
            "{:<8} {:>9} {:>7} {:>8} {:>6} {:>6}  {}",
            resource.as_str(),
            remaining,
            limit,
            reserve,
            used_minute,
            used_hour,
            resets
        ));
    }
    human.push(String::new());
    human.push(format!(
        "All-agent windows: {minute}/{} per minute, {hour}/{} per hour, search {search_minute}/{} per minute",
        windows.per_minute, windows.per_hour, windows.search_per_minute
    ));

    let data = json!({
        "resources": resources,
        "windows": {
            "per_minute": { "used": minute, "limit": windows.per_minute },
            "per_hour": { "used": hour, "limit": windows.per_hour },
            "search_per_minute": { "used": search_minute, "limit": windows.search_per_minute },
        },
    });
    let mut report = Report::new(data).human(human);
    report.meta.quota = snapshots;
    Ok(report)
}
