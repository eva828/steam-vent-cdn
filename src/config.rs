use std::time::Duration;

#[derive(Debug, Clone)]
pub struct DownloaderConfig {
    pub username: Option<String>,
    pub password: Option<String>,
    pub timeout: Duration,
    pub max_retries: u32,
    pub retry_delay: Duration,
}

impl Default for DownloaderConfig {
    fn default() -> Self {
        Self {
            username: None,
            password: None,
            timeout: Duration::from_secs(30),
            max_retries: 3,
            retry_delay: Duration::from_secs(1),
        }
    }
}

impl DownloaderConfig {
    pub fn new(username: Option<String>, password: Option<String>) -> Self {
        Self {
            username,
            password,
            ..Default::default()
        }
    }
}
