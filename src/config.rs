use std::time::Duration;

#[derive(Debug, Clone)]
pub enum Credentials {
    Anonymous,
    UserPass { username: String, password: String },
}

impl Default for Credentials {
    fn default() -> Self {
        Self::Anonymous
    }
}

#[derive(Debug, Clone)]
pub struct DownloaderConfig {
    pub credentials: Credentials,
    pub timeout: Duration,
    pub max_retries: u32,
    pub retry_delay: Duration,
    pub app_id: u32,
    pub concurrent_downloads: usize,
}

impl Default for DownloaderConfig {
    fn default() -> Self {
        Self {
            credentials: Credentials::default(),
            timeout: Duration::from_secs(30),
            max_retries: 3,
            retry_delay: Duration::from_secs(1),
            app_id: 1281930,
            concurrent_downloads: 8,
        }
    }
}

impl DownloaderConfig {
    pub fn new(credentials: Credentials) -> Self {
        Self {
            credentials,
            ..Default::default()
        }
    }

    pub fn with_app_id(mut self, app_id: Option<u32>) -> Self {
        if let Some(id) = app_id {
            self.app_id = id;
        }
        self
    }

    pub fn with_concurrent_downloads(mut self, concurrent_downloads: Option<usize>) -> Self {
        if let Some(c) = concurrent_downloads {
            self.concurrent_downloads = c;
        }
        self
    }
}
