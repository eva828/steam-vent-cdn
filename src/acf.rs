//! Generation of Steam ACF (app manifest / workshop manifest) files.
//!
//! The generated files mirror the structure Steam writes, so the downloaded
//! content can be recognised by the Steam client. When a file already exists its
//! entries are preserved and only the affected items are updated.

use std::path::Path;

use crate::downloader::vdf::{Vdf, VdfObject, VdfValue, parse, to_text};
use crate::error::{Result, SteamVentError};

/// `StateFlags` value Steam uses for a fully installed app.
const STATE_FLAGS_FULLY_INSTALLED: u32 = 4;
const UNIVERSE_PUBLIC: u32 = 1;
const DEFAULT_LANGUAGE: &str = "english";

/// A single depot entry written to `InstalledDepots`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledDepot {
    pub depot_id: u32,
    pub manifest_id: u64,
    pub size: u64,
    pub dlc_app_id: Option<u32>,
}

/// Data needed to write `steamapps/appmanifest_<appid>.acf`.
///
/// `SizeOnDisk` is recomputed from the merged `InstalledDepots` entries,
/// so repeated runs accumulate correctly instead of reflecting only the
/// latest download.
#[derive(Debug, Clone)]
pub struct AppManifest {
    pub app_id: u32,
    pub name: String,
    pub installdir: String,
    pub build_id: u64,
    pub last_updated: u64,
    pub bytes_to_download: u64,
    /// SteamID64 of the account that owns the install, or 0 for anonymous.
    pub last_owner: u64,
    pub installed_depots: Vec<InstalledDepot>,
    /// Shared depots as `(depot_id, owner_app_id)`.
    pub shared_depots: Vec<(u32, u32)>,
}

impl AppManifest {
    pub async fn write(&self, path: &Path) -> Result<()> {
        let mut root = match read_root(path).await? {
            Some(root) => root,
            None => VdfObject::new(),
        };

        root.insert("appid", VdfValue::string(self.app_id.to_string()));
        root.insert("Universe", VdfValue::string(UNIVERSE_PUBLIC.to_string()));
        root.insert("name", VdfValue::string(self.name.clone()));
        root.insert(
            "StateFlags",
            VdfValue::string(STATE_FLAGS_FULLY_INSTALLED.to_string()),
        );
        root.insert("installdir", VdfValue::string(self.installdir.clone()));
        root.insert(
            "LastUpdated",
            VdfValue::string(self.last_updated.to_string()),
        );
        root.insert("UpdateResult", VdfValue::string("0"));
        root.insert("buildid", VdfValue::string(self.build_id.to_string()));
        root.insert("LastOwner", VdfValue::string(self.last_owner.to_string()));
        root.insert(
            "BytesToDownload",
            VdfValue::string(self.bytes_to_download.to_string()),
        );
        root.insert(
            "BytesDownloaded",
            VdfValue::string(self.bytes_to_download.to_string()),
        );
        root.insert("TargetBuildID", VdfValue::string(self.build_id.to_string()));
        root.insert("AutoUpdateBehavior", VdfValue::string("0"));
        root.insert("AllowOtherDownloadsWhileRunning", VdfValue::string("0"));

        let installed: Vec<(String, VdfValue)> = self
            .installed_depots
            .iter()
            .map(|depot| {
                let mut entry = VdfObject::new();
                entry.insert("manifest", VdfValue::string(depot.manifest_id.to_string()));
                entry.insert("size", VdfValue::string(depot.size.to_string()));
                if let Some(dlc_app_id) = depot.dlc_app_id {
                    entry.insert("dlcappid", VdfValue::string(dlc_app_id.to_string()));
                }
                (depot.depot_id.to_string(), VdfValue::Object(entry))
            })
            .collect();
        merge_object(&mut root, "InstalledDepots", installed);

        let shared: Vec<(String, VdfValue)> = self
            .shared_depots
            .iter()
            .map(|(depot_id, owner)| (depot_id.to_string(), VdfValue::string(owner.to_string())))
            .collect();
        merge_object(&mut root, "SharedDepots", shared);

        let size_on_disk = installed_depots_size(&root);
        root.insert("SizeOnDisk", VdfValue::string(size_on_disk.to_string()));

        ensure_language(&mut root, "UserConfig");
        ensure_language(&mut root, "MountedConfig");

        write_document(path, "AppState", root).await
    }
}

/// A single installed workshop item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkshopItem {
    pub item_id: u64,
    pub manifest_id: u64,
    pub size: u64,
    pub time_updated: u64,
    pub time_touched: u64,
}

/// Data needed to write `steamapps/workshop/appworkshop_<appid>.acf`.
#[derive(Debug, Clone)]
pub struct WorkshopManifest {
    pub app_id: u32,
    pub last_updated: u64,
    pub last_app_ran: u64,
    /// Steam account ID (32 bit) that subscribed to the items, or 0.
    pub subscribed_by: u32,
    pub items: Vec<WorkshopItem>,
}

impl WorkshopManifest {
    pub async fn write(&self, path: &Path) -> Result<()> {
        let mut root = match read_root(path).await? {
            Some(root) => root,
            None => VdfObject::new(),
        };

        root.insert("appid", VdfValue::string(self.app_id.to_string()));
        root.insert("NeedsUpdate", VdfValue::string("0"));
        root.insert("NeedsDownload", VdfValue::string("0"));
        root.insert(
            "TimeLastUpdated",
            VdfValue::string(self.last_updated.to_string()),
        );
        root.insert(
            "TimeLastAppRan",
            VdfValue::string(self.last_app_ran.to_string()),
        );

        let installed: Vec<(String, VdfValue)> = self
            .items
            .iter()
            .map(|item| {
                let mut entry = VdfObject::new();
                entry.insert("size", VdfValue::string(item.size.to_string()));
                entry.insert(
                    "timeupdated",
                    VdfValue::string(item.time_updated.to_string()),
                );
                entry.insert("manifest", VdfValue::string(item.manifest_id.to_string()));
                (item.item_id.to_string(), VdfValue::Object(entry))
            })
            .collect();
        merge_object(&mut root, "WorkshopItemsInstalled", installed);

        let details: Vec<(String, VdfValue)> = self
            .items
            .iter()
            .map(|item| {
                let mut entry = VdfObject::new();
                entry.insert("manifest", VdfValue::string(item.manifest_id.to_string()));
                entry.insert(
                    "timeupdated",
                    VdfValue::string(item.time_updated.to_string()),
                );
                entry.insert(
                    "timetouched",
                    VdfValue::string(item.time_touched.to_string()),
                );
                entry.insert(
                    "subscribedby",
                    VdfValue::string(self.subscribed_by.to_string()),
                );
                (item.item_id.to_string(), VdfValue::Object(entry))
            })
            .collect();
        merge_object(&mut root, "WorkshopItemDetails", details);

        let size_on_disk: u64 = root
            .get("WorkshopItemsInstalled")
            .and_then(VdfValue::as_object)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|(_, value)| {
                        value
                            .as_object()
                            .and_then(|entry| entry.get("size"))
                            .and_then(VdfValue::as_u64)
                    })
                    .sum()
            })
            .unwrap_or_default();
        root.insert("SizeOnDisk", VdfValue::string(size_on_disk.to_string()));

        write_document(path, "AppWorkshop", root).await
    }
}

async fn read_root(path: &Path) -> Result<Option<VdfObject>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            if bytes.iter().all(|b| b.is_ascii_whitespace() || *b == 0) {
                return Ok(None);
            }
            let document = parse(&bytes)?;
            let root = document
                .value
                .as_object()
                .ok_or_else(|| {
                    SteamVentError::from(crate::error::VdfError::ExpectedObject {
                        found: document.value.type_name(),
                    })
                })?
                .clone();
            Ok(Some(root))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn merge_object(root: &mut VdfObject, key: &str, entries: Vec<(String, VdfValue)>) {
    let existing = root.get(key).and_then(VdfValue::as_object).cloned();
    if entries.is_empty() && existing.is_none() {
        return;
    }

    let mut object = existing.unwrap_or_default();
    for (entry_key, value) in entries {
        object.insert(entry_key, value);
    }
    root.insert(key, VdfValue::Object(object));
}

fn installed_depots_size(root: &VdfObject) -> u64 {
    root.get("InstalledDepots")
        .and_then(VdfValue::as_object)
        .map(|depots| {
            depots
                .iter()
                .filter_map(|(_, value)| {
                    value
                        .as_object()
                        .and_then(|entry| entry.get("size"))
                        .and_then(VdfValue::as_u64)
                })
                .sum()
        })
        .unwrap_or_default()
}

fn ensure_language(root: &mut VdfObject, key: &str) {
    let mut object = root
        .get(key)
        .and_then(VdfValue::as_object)
        .cloned()
        .unwrap_or_default();
    if object.get("language").and_then(VdfValue::as_str).is_none() {
        object.insert("language", VdfValue::string(DEFAULT_LANGUAGE));
    }
    root.insert(key, VdfValue::Object(object));
}

async fn write_document(path: &Path, key: &str, root: VdfObject) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let document = Vdf {
        key: key.to_string(),
        value: VdfValue::Object(root),
    };
    tokio::fs::write(path, to_text(&document)).await?;
    tracing::info!("Wrote manifest {:?}", path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("svc-acf-{}-{}", std::process::id(), name))
    }

    fn sample_app() -> AppManifest {
        AppManifest {
            app_id: 480,
            name: "Spacewar".to_string(),
            installdir: "Spacewar".to_string(),
            build_id: 3538192,
            last_updated: 1_700_000_000,
            bytes_to_download: 400,
            last_owner: 0,
            installed_depots: vec![InstalledDepot {
                depot_id: 481,
                manifest_id: 123,
                size: 1000,
                dlc_app_id: None,
            }],
            shared_depots: vec![],
        }
    }

    #[tokio::test]
    async fn writes_app_manifest() {
        let path = temp_path("app.acf");
        let _ = tokio::fs::remove_file(&path).await;
        sample_app().write(&path).await.unwrap();

        let text = tokio::fs::read_to_string(&path).await.unwrap();
        let document = parse(text.as_bytes()).unwrap();
        assert_eq!(document.key, "AppState");
        assert_eq!(document.get_path(&["appid"]).unwrap().as_u64(), Some(480));
        assert_eq!(
            document.get_path(&["installdir"]).unwrap().as_str(),
            Some("Spacewar")
        );
        assert_eq!(
            document.get_path(&["StateFlags"]).unwrap().as_u64(),
            Some(4)
        );
        assert_eq!(
            document
                .get_path(&["InstalledDepots", "481", "manifest"])
                .unwrap()
                .as_u64(),
            Some(123)
        );
        assert_eq!(
            document.get_path(&["SizeOnDisk"]).unwrap().as_u64(),
            Some(1000)
        );
        assert_eq!(
            document
                .get_path(&["UserConfig", "language"])
                .unwrap()
                .as_str(),
            Some("english")
        );
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn merges_existing_app_manifest() {
        let path = temp_path("merge.acf");
        let _ = tokio::fs::remove_file(&path).await;
        sample_app().write(&path).await.unwrap();

        let mut second = sample_app();
        second.installed_depots = vec![InstalledDepot {
            depot_id: 482,
            manifest_id: 456,
            size: 200,
            dlc_app_id: Some(999),
        }];
        second.write(&path).await.unwrap();

        let text = tokio::fs::read_to_string(&path).await.unwrap();
        let document = parse(text.as_bytes()).unwrap();
        assert_eq!(
            document
                .get_path(&["InstalledDepots", "481", "manifest"])
                .unwrap()
                .as_u64(),
            Some(123)
        );
        assert_eq!(
            document
                .get_path(&["InstalledDepots", "482", "manifest"])
                .unwrap()
                .as_u64(),
            Some(456)
        );
        assert_eq!(
            document
                .get_path(&["InstalledDepots", "482", "dlcappid"])
                .unwrap()
                .as_u64(),
            Some(999)
        );
        assert_eq!(
            document.get_path(&["SizeOnDisk"]).unwrap().as_u64(),
            Some(1200)
        );
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn preserves_user_config_keys() {
        use crate::downloader::vdf::VdfObject;
        use crate::downloader::vdf::{Vdf, VdfValue, to_text};

        let path = temp_path("userconfig.acf");
        let _ = tokio::fs::remove_file(&path).await;
        let mut root = VdfObject::new();
        let mut user_config = VdfObject::new();
        user_config.insert("language", VdfValue::string("japanese"));
        user_config.insert("gametype", VdfValue::string("custom"));
        root.insert("UserConfig", VdfValue::Object(user_config));
        let document = Vdf {
            key: "AppState".to_string(),
            value: VdfValue::Object(root),
        };
        tokio::fs::write(&path, to_text(&document)).await.unwrap();

        sample_app().write(&path).await.unwrap();

        let text = tokio::fs::read_to_string(&path).await.unwrap();
        let document = parse(text.as_bytes()).unwrap();
        assert_eq!(
            document
                .get_path(&["UserConfig", "language"])
                .unwrap()
                .as_str(),
            Some("japanese")
        );
        assert_eq!(
            document
                .get_path(&["UserConfig", "gametype"])
                .unwrap()
                .as_str(),
            Some("custom")
        );
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn treats_empty_acf_as_new() {
        let path = temp_path("empty.acf");
        tokio::fs::write(&path, b"").await.unwrap();
        sample_app().write(&path).await.unwrap();

        let text = tokio::fs::read_to_string(&path).await.unwrap();
        let document = parse(text.as_bytes()).unwrap();
        assert_eq!(document.key, "AppState");
        assert_eq!(document.get_path(&["appid"]).unwrap().as_u64(), Some(480));
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn writes_workshop_manifest_and_merges() {
        let path = temp_path("workshop.acf");
        let _ = tokio::fs::remove_file(&path).await;

        let first = WorkshopManifest {
            app_id: 4000,
            last_updated: 100,
            last_app_ran: 200,
            subscribed_by: 0,
            items: vec![WorkshopItem {
                item_id: 160250458,
                manifest_id: 555,
                size: 1000,
                time_updated: 100,
                time_touched: 200,
            }],
        };
        first.write(&path).await.unwrap();

        let second = WorkshopManifest {
            app_id: 4000,
            last_updated: 300,
            last_app_ran: 400,
            subscribed_by: 42,
            items: vec![WorkshopItem {
                item_id: 123456,
                manifest_id: 777,
                size: 500,
                time_updated: 300,
                time_touched: 400,
            }],
        };
        second.write(&path).await.unwrap();

        let text = tokio::fs::read_to_string(&path).await.unwrap();
        let document = parse(text.as_bytes()).unwrap();
        assert_eq!(document.key, "AppWorkshop");
        assert_eq!(
            document
                .get_path(&["WorkshopItemsInstalled", "160250458", "manifest"])
                .unwrap()
                .as_u64(),
            Some(555)
        );
        assert_eq!(
            document
                .get_path(&["WorkshopItemsInstalled", "123456", "manifest"])
                .unwrap()
                .as_u64(),
            Some(777)
        );
        assert_eq!(
            document.get_path(&["SizeOnDisk"]).unwrap().as_u64(),
            Some(1500)
        );
        assert_eq!(
            document.get_path(&["NeedsUpdate"]).unwrap().as_u64(),
            Some(0)
        );
        let _ = tokio::fs::remove_file(&path).await;
    }
}
