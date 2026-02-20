use crate::config::DownloaderConfig;
use crate::error::{Result, SteamVentError};
use std::time::Duration;
use steam_vent::ServerList;
use steam_vent::auth::{FileGuardDataStore, UserProvidedAuthConfirmationHandler};
use steam_vent::connection::Connection;

pub struct SteamClient {
    config: DownloaderConfig,
}

pub struct SteamSession {
    pub connection: Connection,
    config: DownloaderConfig,
}

impl SteamClient {
    pub fn new(config: DownloaderConfig) -> Self {
        Self { config }
    }

    pub async fn connect(self) -> Result<SteamSession> {
        let server_list = ServerList::discover()
            .await
            .map_err(|e| SteamVentError::Connection(e.to_string()))?;

        let mut connection = match &self.config.credentials {
            crate::config::Credentials::Anonymous => {
                log::info!("Connecting anonymously to Steam...");
                Connection::anonymous(&server_list)
                    .await
                    .map_err(|e| SteamVentError::Connection(e.to_string()))?
            }
            crate::config::Credentials::UserPass { username, password } => {
                log::info!("Logging in as {}...", username);
                let auth_handler = UserProvidedAuthConfirmationHandler::new(
                    tokio::io::stdin(),
                    tokio::io::stdout(),
                );
                let guard_store = FileGuardDataStore::user_cache();

                Connection::login(&server_list, username, password, guard_store, auth_handler)
                    .await
                    .map_err(|e| SteamVentError::Connection(e.to_string()))?
            }
        };

        connection.set_timeout(self.config.timeout);

        // SteamのLogonシーケンスが完了し、セッションが完全に確立されるのを待機するために必要です。
        // （待機せずに後続のリクエストを送信すると、NotLoggedOn等で弾かれる可能性があります）
        tokio::time::sleep(Duration::from_millis(500)).await;

        log::info!("Connected.");
        Ok(SteamSession {
            connection,
            config: self.config,
        })
    }
}

impl SteamSession {
    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }

    pub fn config(&self) -> &DownloaderConfig {
        &self.config
    }
}
