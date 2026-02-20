use crate::config::DownloaderConfig;
use crate::error::{Result, SteamVentError};
use std::time::Duration;
use steam_vent::ServerList;
use steam_vent::auth::{FileGuardDataStore, UserProvidedAuthConfirmationHandler};
use steam_vent::connection::Connection;

pub struct SteamClient {
    connection: Option<Connection>,
    server_list: ServerList,
    config: DownloaderConfig,
}

impl SteamClient {
    pub async fn new(config: DownloaderConfig) -> Result<Self> {
        let server_list = ServerList::discover()
            .await
            .map_err(|e| SteamVentError::Connection(e.to_string()))?;

        Ok(Self {
            connection: None,
            server_list,
            config,
        })
    }

    pub async fn connect_anonymous(&mut self) -> Result<()> {
        log::info!("Connecting anonymously to Steam...");

        let mut connection = Connection::anonymous(&self.server_list)
            .await
            .map_err(|e| SteamVentError::Connection(e.to_string()))?;

        connection.set_timeout(self.config.timeout);

        tokio::time::sleep(Duration::from_millis(500)).await;

        self.connection = Some(connection);
        log::info!("Connected.");
        Ok(())
    }

    pub async fn login(&mut self) -> Result<()> {
        let username = self
            .config
            .username
            .as_deref()
            .ok_or_else(|| SteamVentError::Connection("No username provided".to_string()))?;
        let password = self
            .config
            .password
            .as_deref()
            .ok_or_else(|| SteamVentError::Connection("No password provided".to_string()))?;

        log::info!("Logging in as {}...", username);

        let auth_handler =
            UserProvidedAuthConfirmationHandler::new(tokio::io::stdin(), tokio::io::stdout());
        let guard_store = FileGuardDataStore::user_cache();

        let mut connection = Connection::login(
            &self.server_list,
            username,
            password,
            guard_store,
            auth_handler,
        )
        .await
        .map_err(|e| SteamVentError::Connection(e.to_string()))?;

        connection.set_timeout(self.config.timeout);

        tokio::time::sleep(Duration::from_millis(500)).await;

        self.connection = Some(connection);
        log::info!("Logged in.");
        Ok(())
    }

    pub fn connection_mut(&mut self) -> Result<&mut Connection> {
        self.connection
            .as_mut()
            .ok_or_else(|| SteamVentError::Connection("Not connected".to_string()))
    }
}
