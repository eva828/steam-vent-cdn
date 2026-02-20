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
}

impl Default for DownloaderConfig {
    fn default() -> Self {
        Self {
            credentials: Credentials::default(),
            timeout: Duration::from_secs(30),
            max_retries: 3,
            retry_delay: Duration::from_secs(1),
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
}
