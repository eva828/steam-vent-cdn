use std::time::Duration;
use steam_vent::ServerList;
use steam_vent::connection::Connection;
use crate::error::{SteamVentError, Result};

pub struct SteamClient {
    connection: Option<Connection>,
    server_list: ServerList,
}

impl SteamClient {
    pub async fn new() -> Result<Self> {
        let server_list = ServerList::discover().await
            .map_err(|e| SteamVentError::Connection(e.to_string()))?;

        Ok(Self {
            connection: None,
            server_list,
        })
    }

    pub async fn connect_anonymous(&mut self) -> Result<()> {
        log::info!("Connecting anonymously to Steam...");
        
        let mut connection = Connection::anonymous(&self.server_list).await
            .map_err(|e| SteamVentError::Connection(e.to_string()))?;
        
        connection.set_timeout(Duration::from_secs(30));
        
        tokio::time::sleep(Duration::from_millis(500)).await;
        
        self.connection = Some(connection);
        log::info!("Connected.");
        Ok(())
    }

    pub fn connection_mut(&mut self) -> Result<&mut Connection> {
        self.connection
            .as_mut()
            .ok_or_else(|| SteamVentError::Connection("Not connected".to_string()))
    }
}
