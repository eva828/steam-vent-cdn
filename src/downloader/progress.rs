pub trait ProgressCallback: Send + Sync {
    fn on_download_progress(&self, bytes: u64);
    fn on_decode_progress(&self, bytes: u64);
    fn on_write_progress(&self, bytes: u64);
}
