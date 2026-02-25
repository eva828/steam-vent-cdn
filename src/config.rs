use std::time::Duration;

#[derive(Clone, Default)]
pub enum Credentials {
    #[default]
    Anonymous,
    UserPass {
        username: String,
        password: String,
    },
}

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub retry_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            retry_delay: Duration::from_secs(1),
        }
    }
}

#[derive(Clone)]
pub struct Config {
    pub credentials: Credentials,
    pub timeout: Duration,
    pub retry_policy: RetryPolicy,
    pub app_id: u32,
    pub parallel_download: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            credentials: Credentials::default(),
            timeout: Duration::from_secs(30),
            retry_policy: RetryPolicy::default(),
            app_id: 1281930,
            parallel_download: 16,
        }
    }
}

pub struct ConfigBuilder {
    config: Config,
}

impl Default for ConfigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigBuilder {
    pub fn new() -> Self {
        Self {
            config: Config::default(),
        }
    }

    pub fn credentials(mut self, credentials: Credentials) -> Self {
        self.config.credentials = credentials;
        self
    }

    pub fn app_id(mut self, app_id: u32) -> Self {
        self.config.app_id = app_id;
        self
    }

    pub fn parallel_download(mut self, parallel_download: usize) -> Self {
        self.config.parallel_download = parallel_download;
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.config.timeout = timeout;
        self
    }

    pub fn retry_policy(mut self, retry_policy: RetryPolicy) -> Self {
        self.config.retry_policy = retry_policy;
        self
    }

    pub fn build(self) -> Config {
        self.config
    }
}

impl Config {
    pub fn builder() -> ConfigBuilder {
        ConfigBuilder::new()
    }
}
