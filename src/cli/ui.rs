use steam_vent_cdn::downloader::steam_api::VersionInfo;
use time::OffsetDateTime;
use time::macros::format_description;

fn format_timestamp(timestamp: u32) -> String {
    static FORMAT: &[time::format_description::FormatItem<'static>] =
        format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");

    OffsetDateTime::from_unix_timestamp(timestamp as i64)
        .ok()
        .and_then(|dt| dt.format(FORMAT).ok())
        .unwrap_or_else(|| "Unknown".to_string())
}

pub fn display_version_history(item_id: u64, versions: &[VersionInfo]) {
    tracing::info!("Workshop item {} version history:", item_id);
    tracing::info!(
        "{:<20} | {:<20} | {:<20} | Description",
        "Timestamp",
        "Manifest ID",
        "Date"
    );
    tracing::info!(
        "{:-<20}-+-{:-<20}-+-{:-<20}-+-{}",
        "--------------------",
        "--------------------",
        "--------------------",
        "------------------------"
    );

    for (i, version) in versions.iter().enumerate() {
        let date = format_timestamp(version.timestamp);
        let latest_mark = if i == 0 { " [LATEST]" } else { "" };
        let description = if version.description.is_empty() {
            "No description".to_string()
        } else {
            format!("{}{}", version.description, latest_mark)
        };
        tracing::info!(
            "{:<20} | {:<20} | {:<20} | {}",
            version.timestamp,
            version.manifest_id,
            date,
            description
        );
    }

    tracing::info!("Total: {} versions", versions.len());
}
