use steam_vent_cdn::{DownloadItem, VersionInfo};

fn format_timestamp(timestamp: u32) -> String {
    use chrono::{DateTime, Utc};
    let datetime = DateTime::<Utc>::from_timestamp(timestamp as i64, 0);
    datetime
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "Unknown".to_string())
}

pub fn format_file_size(size: u64) -> String {
    if size < 1024 {
        format!("{} B", size)
    } else if size < 1024 * 1024 {
        format!("{:.2} KB", size as f64 / 1024.0)
    } else {
        format!("{:.2} MB", size as f64 / (1024.0 * 1024.0))
    }
}

pub fn display_file_list(item_id: u64, files: &[DownloadItem]) {
    tracing::info!("Workshop item {} files:", item_id);
    for file in files {
        let size = file.manifest_info.size.unwrap_or_else(|| {
            file.manifest_info
                .chunks
                .iter()
                .map(|c| c.uncompressed_size as u64)
                .sum()
        });

        let size_str = format_file_size(size);
        tracing::info!("  - {} ({})", file.filename, size_str);
    }

    tracing::info!("Total: {} files", files.len());
}

pub fn display_version_history(item_id: u64, versions: &[VersionInfo]) {
    tracing::info!("Workshop item {} version history:", item_id);
    tracing::info!("{:<20} | {:<20} | Description", "Timestamp", "Date");
    tracing::info!(
        "{:-<20}-+-{:-<20}-+-{}",
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
        tracing::info!("{:<20} | {:<20} | {}", version.timestamp, date, description);
    }

    tracing::info!("Total: {} versions", versions.len());
}
