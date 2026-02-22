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

impl Credentials {
    pub fn from_options(username: Option<String>, password: Option<String>) -> Self {
        match (username, password) {
            (Some(username), Some(password)) => Self::UserPass { username, password },
            (Some(username), None) => {
                let password = rpassword::prompt_password("Password: ").unwrap_or_default();
                Self::UserPass { username, password }
            }
            _ => Self::Anonymous,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DownloaderConfig {
    pub credentials: Credentials,
    pub timeout: Duration,
    pub max_retries: u32,
    pub retry_delay: Duration,
    pub app_id: u32,
    pub parallel_download: usize,
    pub parallel_decode: usize,
}

impl Default for DownloaderConfig {
    fn default() -> Self {
        Self {
            credentials: Credentials::default(),
            timeout: Duration::from_secs(30),
            max_retries: 3,
            retry_delay: Duration::from_secs(1),
            app_id: 1281930,
            parallel_download: 16,
            parallel_decode: 8,
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

    pub fn with_app_id(mut self, app_id: u32) -> Self {
        self.app_id = app_id;
        self
    }

    pub fn with_parallel_download(mut self, parallel_download: usize) -> Self {
        self.parallel_download = parallel_download;
        self
    }

    pub fn with_parallel_decode(mut self, parallel_decode: usize) -> Self {
        self.parallel_decode = parallel_decode;
        self
    }
}
