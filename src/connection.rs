use crate::config::{Config, Credentials};
use crate::error::{Result, SteamSessionError};

use steam_vent::ServerList;
use steam_vent::auth::{FileGuardDataStore, UserProvidedAuthConfirmationHandler};
use steam_vent::connection::Connection;

pub struct SteamClient {
    config: Config,
}

pub struct SteamSession {
    connection: Connection,
    config: Config,
}

impl SteamClient {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub async fn connect(&self) -> Result<SteamSession> {
        let server_list = ServerList::discover()
            .await
            .map_err(SteamSessionError::ServerDiscovery)?;

        let mut connection = match &self.config.credentials {
            Credentials::Anonymous => {
                tracing::info!("Connecting anonymously to Steam");
                Connection::anonymous(&server_list)
                    .await
                    .map_err(|e| SteamSessionError::SessionInit(e.to_string()))?
            }
            Credentials::UserPass { username, password } => {
                tracing::info!("Logging in as {}", username);
                let auth_handler = UserProvidedAuthConfirmationHandler::new(
                    tokio::io::stdin(),
                    tokio::io::stdout(),
                );
                let guard_store = FileGuardDataStore::user_cache();

                Connection::login(&server_list, username, password, guard_store, auth_handler)
                    .await
                    .map_err(|e| SteamSessionError::LoginFailed(e.to_string()))?
            }
        };

        connection.set_timeout(self.config.timeout);

        tracing::info!("Connected.");
        Ok(SteamSession {
            connection,
            config: self.config.clone(),
        })
    }
}

impl SteamSession {
    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }

    pub fn config(&self) -> &Config {
        &self.config
    }
}

macro_rules! retry_loop {
    ($fut:expr, $max_retries:expr, $retry_delay:expr) => {{
        let mut retry_count = 0;
        loop {
            match ($fut).await {
                Ok(resp) => break Ok(resp),
                Err(e) if retry_count < $max_retries && is_transient(&e) => {
                    tracing::debug!(
                        "Transient error, retrying (attempt {}): {}",
                        retry_count + 1,
                        e,
                    );
                    tokio::time::sleep($retry_delay).await;
                    retry_count += 1;
                }
                Err(e) => break Err(SteamSessionError::from(e).into()),
            }
        }
    }};
}

fn is_transient(err: &steam_vent::NetworkError) -> bool {
    use steam_vent::EResult;
    use steam_vent::NetworkError;
    matches!(
        err,
        NetworkError::IO(_)
            | NetworkError::Ws(_)
            | NetworkError::Timeout
            | NetworkError::EOF
            | NetworkError::ApiError(
                EResult::Timeout
                    | EResult::ServiceUnavailable
                    | EResult::TryAnotherCM
                    | EResult::NoConnection
                    | EResult::Busy
                    | EResult::RateLimitExceeded,
            )
    )
}

pub(crate) trait ConnectionExt: steam_vent::ConnectionTrait + Sync {
    fn service_method_with_retry<'a, R>(
        &'a self,
        req: R,
        max_retries: u32,
        retry_delay: std::time::Duration,
    ) -> impl std::future::Future<Output = Result<R::Response>> + Send + 'a
    where
        R: steam_vent::ServiceMethodRequest + Clone + Send + 'static,
        R::Response: Send,
    {
        async move { retry_loop!(self.service_method(req.clone()), max_retries, retry_delay) }
    }

    fn job_with_retry<'a, Msg, Rsp>(
        &'a self,
        msg: Msg,
        max_retries: u32,
        retry_delay: std::time::Duration,
    ) -> impl std::future::Future<Output = Result<Rsp>> + Send + 'a
    where
        Msg: steam_vent::NetMessage + Clone + Send + 'static,
        Rsp: steam_vent::NetMessage + Send,
    {
        async move { retry_loop!(self.job(msg.clone()), max_retries, retry_delay) }
    }
}

impl<T: steam_vent::ConnectionTrait + Send + Sync> ConnectionExt for T {}
