use steam_vent_cdn::DownloadItem;

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
