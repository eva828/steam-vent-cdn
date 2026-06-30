pub trait ProgressCallback: Send + Sync {
    fn on_download_progress(&self, bytes: u64);
    fn on_decode_progress(&self, bytes: u64);
    fn on_write_progress(&self, bytes: u64);
}

/// A progress callback that discards all reports.
///
/// Used by tests that do not need progress feedback.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NoopProgress;

#[cfg(test)]
impl ProgressCallback for NoopProgress {
    fn on_download_progress(&self, _bytes: u64) {}
    fn on_decode_progress(&self, _bytes: u64) {}
    fn on_write_progress(&self, _bytes: u64) {}
}
