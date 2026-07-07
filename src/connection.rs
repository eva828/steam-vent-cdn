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
        async move {
            let mut retry_count = 0;
            loop {
                let e = match self.service_method(req.clone()).await {
                    Ok(resp) => return Ok(resp),
                    Err(e) => e,
                };

                match &e {
                    steam_vent::NetworkError::ApiError(
                        steam_vent::EResult::NotLoggedOn | steam_vent::EResult::AccessDenied,
                    ) if retry_count < max_retries => {
                        tracing::debug!(
                            "Login not fully established, retrying {} (attempt {}): {}",
                            std::any::type_name::<R>(),
                            retry_count + 1,
                            e
                        );
                        tokio::time::sleep(retry_delay).await;
                        retry_count += 1;
                    }
                    _ => return Err(SteamSessionError::from(e).into()),
                }
            }
        }
    }
}

impl<T: steam_vent::ConnectionTrait + Send + Sync> ConnectionExt for T {}
