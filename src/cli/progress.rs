use indicatif::MultiProgress;
use std::path::Path;
use std::time::Instant;
use steam_vent_cdn::{DownloadItem, ProgressCallback};

pub struct ProgressManager {
    pb_download: indicatif::ProgressBar,
    pb_decode: indicatif::ProgressBar,
    pb_write: indicatif::ProgressBar,
    start_time: Instant,
}

impl ProgressManager {
    pub fn new(multi: &MultiProgress, files: &[DownloadItem]) -> Self {
        let total_compressed: u64 = files
            .iter()
            .map(|f| {
                f.chunks
                    .iter()
                    .map(|c| c.compressed_size as u64)
                    .sum::<u64>()
            })
            .sum();

        let total_decompressed: u64 = files
            .iter()
            .map(|f| {
                f.chunks
                    .iter()
                    .map(|c| c.uncompressed_size as u64)
                    .sum::<u64>()
            })
            .sum();

        tracing::info!(
            "Total sizes: {} compressed ({} MiB), {} decompressed ({} MiB)",
            total_compressed,
            total_compressed / 1024 / 1024,
            total_decompressed,
            total_decompressed / 1024 / 1024
        );

        let pb_download = multi.add(indicatif::ProgressBar::new(total_compressed));
        pb_download.set_style(
            indicatif::ProgressStyle::with_template(
                "{spinner:.blue} {msg:<11} [{elapsed_precise}] [{wide_bar:.blue/blue}] {bytes}/{total_bytes} ({mbps}, {eta})",
            )
            .unwrap()
            .with_key("mbps", |state: &indicatif::ProgressState, w: &mut dyn std::fmt::Write| {
                let mbps = state.per_sec() * 8.0 / 1_000_000.0;
                write!(w, "{:.1} Mbps", mbps).unwrap();
            })
            .progress_chars("#>-"),
        );
        pb_download.set_message("Downloading");
        pb_download.enable_steady_tick(std::time::Duration::from_millis(100));

        let pb_decode = multi.add(indicatif::ProgressBar::new(total_decompressed));
        pb_decode.set_style(
            indicatif::ProgressStyle::with_template(
                "{spinner:.green} {msg:<11} [{elapsed_precise}] [{wide_bar:.green/green}] {bytes}/{total_bytes} ({mbps}, {eta})",
            )
            .unwrap()
            .with_key("mbps", |state: &indicatif::ProgressState, w: &mut dyn std::fmt::Write| {
                let mbps = state.per_sec() * 8.0 / 1_000_000.0;
                write!(w, "{:.1} Mbps", mbps).unwrap();
            })
            .progress_chars("#>-"),
        );
        pb_decode.set_message("Decoding");
        pb_decode.enable_steady_tick(std::time::Duration::from_millis(100));

        let pb_write = multi.add(indicatif::ProgressBar::new(total_decompressed));
        pb_write.set_style(
            indicatif::ProgressStyle::with_template(
                "{spinner:.yellow} {msg:<11} [{elapsed_precise}] [{wide_bar:.yellow/yellow}] {bytes}/{total_bytes} ({mbps}, {eta})",
            )
            .unwrap()
            .with_key("mbps", |state: &indicatif::ProgressState, w: &mut dyn std::fmt::Write| {
                let mbps = state.per_sec() * 8.0 / 1_000_000.0;
                write!(w, "{:.1} Mbps", mbps).unwrap();
            })
            .progress_chars("#>-"),
        );
        pb_write.set_message("Writing");
        pb_write.enable_steady_tick(std::time::Duration::from_millis(100));

        Self {
            pb_download,
            pb_decode,
            pb_write,
            start_time: Instant::now(),
        }
    }

    pub fn increment_download(&self, bytes: u64) {
        self.pb_download.inc(bytes);
    }

    pub fn increment_decode(&self, bytes: u64) {
        self.pb_decode.inc(bytes);
    }

    pub fn increment_write(&self, bytes: u64) {
        self.pb_write.inc(bytes);
    }

    pub fn file_complete(&self, path: &Path, bytes: usize) {
        tracing::info!("Completed file: {} ({} bytes)", path.display(), bytes);
    }

    pub fn finish(&self) {
        let elapsed = self.start_time.elapsed();
        tracing::info!("Total time: {:.2}s", elapsed.as_secs_f64());
        self.pb_download.finish_with_message("Download complete");
        self.pb_decode.finish_with_message("Decode complete");
        self.pb_write.finish_with_message("Write complete");
    }
}

impl ProgressCallback for ProgressManager {
    fn on_download_progress(&self, bytes: u64) {
        self.increment_download(bytes);
    }

    fn on_decode_progress(&self, bytes: u64) {
        self.increment_decode(bytes);
    }

    fn on_write_progress(&self, bytes: u64) {
        self.increment_write(bytes);
    }

    fn on_file_complete(&self, path: &Path, bytes: usize) {
        self.file_complete(path, bytes);
    }
}
