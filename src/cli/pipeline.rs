use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::stream::TryStreamExt;
use indicatif::MultiProgress;
use url::Url;

use steam_vent_cdn::acf::{AppManifest, InstalledDepot, WorkshopItem, WorkshopManifest};
use steam_vent_cdn::downloader::client::CdnDownloader;
use steam_vent_cdn::downloader::{AppDepot, AppInfo, ItemDetails, Manifest, VersionInfo};
use steam_vent_cdn::error::{AppError, Result, WorkshopError};

use super::args::{DepotManifest, Layout, Platform, SharedDepotsMode};
use super::progress::ProgressManager;
use super::{ui, writer};

/// Tracks which files were already written, scoped per install directory.
///
/// Key is `(installdir, filename)` so different apps with different
/// `installdir` values never collide.
type WrittenMap = Arc<Mutex<HashMap<(String, String), u32>>>;

type DepotTargets = (Vec<(u32, u64)>, Vec<AppDepot>);

/// Identity of the logged in session, used to fill ACF fields.
#[derive(Debug, Clone, Copy)]
pub struct SessionIdentity {
    pub steam_id: u64,
    pub account_id: u32,
}

pub async fn run_list_versions_pipeline(downloader: &CdnDownloader, item_id: u64) -> Result<()> {
    let versions: Vec<_> = downloader
        .fetch_versions(item_id, None)
        .try_collect()
        .await?;
    ui::display_version_history(item_id, &versions);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn run_download_pipeline(
    downloader: &CdnDownloader,
    multi: &MultiProgress,
    app_id: u32,
    item_id: u64,
    output: PathBuf,
    timestamp: Option<u32>,
    manifest_id: Option<u64>,
    layout: Layout,
    identity: &SessionIdentity,
) -> Result<()> {
    let version = resolve_version(downloader, item_id, timestamp, manifest_id).await?;

    tracing::info!("Downloading version with timestamp: {}", version.timestamp);
    tracing::info!("Description: {}", version.description);

    let details = downloader.fetch_item_details(item_id).await?;

    let depot_id = details.consumer_appid.unwrap_or(app_id);

    match layout {
        Layout::Depot => {
            let base_path = output
                .join(app_id.to_string())
                .join(item_id.to_string())
                .join(version.timestamp.to_string());

            if let Some(file_url) = &details.file_url {
                let filename = extract_filename(&details, file_url);
                download_web_item(downloader, file_url, &filename, &base_path).await?;
            } else {
                let manifest = downloader
                    .fetch_manifest_by_id(version.manifest_id, depot_id, depot_id)
                    .await?;
                download_cdn_files(&manifest, &base_path, downloader, multi, None).await?;
            }
        }
        Layout::Steam => {
            if details.file_url.is_some() {
                return Err(WorkshopError::WebItemNotSupported(item_id).into());
            }

            let base_path = output
                .join("steamapps")
                .join("workshop")
                .join("content")
                .join(app_id.to_string())
                .join(item_id.to_string());

            let manifest = downloader
                .fetch_manifest_by_id(version.manifest_id, depot_id, depot_id)
                .await?;
            let size = manifest_size(&manifest);
            download_cdn_files(&manifest, &base_path, downloader, multi, None).await?;

            let item = WorkshopItem {
                item_id,
                manifest_id: version.manifest_id,
                size,
                time_updated: details
                    .time_updated
                    .map(u64::from)
                    .unwrap_or(u64::from(version.timestamp)),
                time_touched: now_secs(),
            };
            let acf = WorkshopManifest {
                app_id,
                last_updated: u64::from(version.timestamp),
                last_app_ran: now_secs(),
                subscribed_by: identity.account_id,
                items: vec![item],
            };
            acf.write(
                &output
                    .join("steamapps")
                    .join("workshop")
                    .join(format!("appworkshop_{}.acf", app_id)),
            )
            .await?;
        }
    }

    Ok(())
}

/// Download all depots of an app for the requested platform.
#[allow(clippy::too_many_arguments)]
pub async fn run_app_download_pipeline(
    downloader: &CdnDownloader,
    multi: &MultiProgress,
    app_id: u32,
    output: PathBuf,
    platform: Platform,
    manifest_overrides: &[DepotManifest],
    layout: Layout,
    shared_mode: SharedDepotsMode,
    identity: &SessionIdentity,
) -> Result<()> {
    match layout {
        Layout::Depot => {
            run_app_download_depot(
                downloader,
                multi,
                app_id,
                output,
                platform,
                manifest_overrides,
            )
            .await
        }
        Layout::Steam => {
            run_app_download_steam(
                downloader,
                multi,
                app_id,
                output,
                platform,
                manifest_overrides,
                shared_mode,
                identity,
            )
            .await
        }
    }
}

async fn run_app_download_depot(
    downloader: &CdnDownloader,
    multi: &MultiProgress,
    app_id: u32,
    output: PathBuf,
    platform: Platform,
    manifest_overrides: &[DepotManifest],
) -> Result<()> {
    let info = downloader.fetch_app_info(app_id).await?;
    tracing::info!("App '{}' (build {})", info.name, info.build_id);

    let (targets, shared) =
        build_targets(app_id, &info, platform.oslist_name(), manifest_overrides)?;

    for depot in &shared {
        tracing::warn!(
            "Skipping shared depot {} (provided by app {}); use --layout steam to install it",
            depot.depot_id,
            depot.depot_from_app.unwrap_or(0)
        );
    }

    if targets.is_empty() {
        return Err(AppError::NoDepots(app_id).into());
    }

    let base_path = output
        .join(app_id.to_string())
        .join(info.build_id.to_string());

    for (depot_id, manifest_id) in targets {
        tracing::info!("Downloading depot {} (manifest {})", depot_id, manifest_id);
        let manifest = downloader
            .fetch_manifest_by_id(manifest_id, app_id, depot_id)
            .await?;
        let depot_path = base_path.join(depot_id.to_string());
        download_cdn_files(&manifest, &depot_path, downloader, multi, None).await?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_app_download_steam(
    downloader: &CdnDownloader,
    multi: &MultiProgress,
    app_id: u32,
    output: PathBuf,
    platform: Platform,
    manifest_overrides: &[DepotManifest],
    shared_mode: SharedDepotsMode,
    identity: &SessionIdentity,
) -> Result<()> {
    let os = platform.oslist_name();
    let steamapps = output.join("steamapps");
    let common = steamapps.join("common");
    let written: WrittenMap = Arc::new(Mutex::new(HashMap::new()));

    let mut processed: HashSet<u32> = HashSet::new();
    let mut queue: VecDeque<u32> = VecDeque::new();
    queue.push_back(app_id);

    while let Some(current) = queue.pop_front() {
        if !processed.insert(current) {
            continue;
        }

        let info = downloader.fetch_app_info(current).await?;
        tracing::info!("App '{}' (build {})", info.name, info.build_id);

        let overrides: &[DepotManifest] = if current == app_id {
            manifest_overrides
        } else {
            &[]
        };
        let (targets, shared) = build_targets(current, &info, os, overrides)?;

        if shared_mode == SharedDepotsMode::Skip {
            let skipped: Vec<(u32, u32)> = shared
                .iter()
                .filter_map(|depot| depot.depot_from_app.map(|owner| (depot.depot_id, owner)))
                .collect();
            if !skipped.is_empty() {
                report_skipped_shared(&skipped, &output, platform);
            }
        }

        let installdir = info
            .installdir
            .clone()
            .ok_or(AppError::MissingInstallDir(current))?;
        let base = common.join(&installdir);

        let mut installed = Vec::new();
        let mut bytes_to_download = 0u64;

        for (depot_id, manifest_id) in targets {
            tracing::info!("Downloading depot {} (manifest {})", depot_id, manifest_id);
            let manifest = downloader
                .fetch_manifest_by_id(manifest_id, current, depot_id)
                .await?;
            let size = manifest_size(&manifest);
            let compressed = info
                .depots
                .iter()
                .find(|depot| depot.depot_id == depot_id)
                .map(|depot| depot.size)
                .filter(|size| *size > 0)
                .unwrap_or(size);

            download_cdn_files(
                &manifest,
                &base,
                downloader,
                multi,
                Some((installdir.clone(), depot_id, Arc::clone(&written))),
            )
            .await?;

            installed.push(InstalledDepot {
                depot_id,
                manifest_id,
                size,
                dlc_app_id: info
                    .depots
                    .iter()
                    .find(|depot| depot.depot_id == depot_id)
                    .and_then(|depot| depot.dlc_app_id),
            });
            bytes_to_download += compressed;
        }

        let mut shared_entries = Vec::new();
        for depot in &shared {
            match depot.depot_from_app {
                Some(owner) if shared_mode == SharedDepotsMode::Auto => {
                    shared_entries.push((depot.depot_id, owner));
                    queue.push_back(owner);
                }
                Some(owner) => tracing::debug!(
                    "Not downloading shared depot {} (provided by app {})",
                    depot.depot_id,
                    owner
                ),
                None => {}
            }
        }

        if installed.is_empty() && shared_entries.is_empty() {
            tracing::warn!("App {} has no depots to install for this platform", current);
        }

        let acf = AppManifest {
            app_id: current,
            name: info.name.clone(),
            installdir,
            build_id: info.build_id,
            last_updated: info.build_time.unwrap_or_else(now_secs),
            bytes_to_download,
            last_owner: identity.steam_id,
            installed_depots: installed,
            shared_depots: shared_entries,
        };
        acf.write(&steamapps.join(format!("appmanifest_{}.acf", current)))
            .await?;
    }

    Ok(())
}

/// Split the depots of an app into downloadable targets and shared installs.
///
/// An explicit `--manifest` override forces the depot to be included even
/// when it was filtered out by `--os`. Unknown depot ids and duplicate
/// overrides are rejected instead of failing later with an obscure CDN error.
fn build_targets(
    app_id: u32,
    info: &AppInfo,
    os: Option<&str>,
    overrides: &[DepotManifest],
) -> Result<DepotTargets> {
    let mut seen_override = HashSet::new();
    for entry in overrides {
        if !seen_override.insert(entry.depot_id) {
            return Err(AppError::DuplicateManifestOverride(entry.depot_id).into());
        }
        if info
            .depots
            .iter()
            .all(|depot| depot.depot_id != entry.depot_id)
        {
            return Err(AppError::UnknownDepot {
                app_id,
                depot_id: entry.depot_id,
            }
            .into());
        }
    }

    let mut targets = Vec::new();
    let mut shared = Vec::new();

    for depot in info.depots_for_os(os) {
        if depot.depot_from_app.is_some() {
            shared.push(depot.clone());
            continue;
        }

        let manifest_id = overrides
            .iter()
            .find(|entry| entry.depot_id == depot.depot_id)
            .map(|entry| entry.manifest_id)
            .unwrap_or(depot.manifest_id);
        targets.push((depot.depot_id, manifest_id));
    }

    for entry in overrides {
        if targets
            .iter()
            .any(|(depot_id, _)| *depot_id == entry.depot_id)
        {
            continue;
        }
        let depot = info
            .depots
            .iter()
            .find(|depot| depot.depot_id == entry.depot_id)
            .expect("override validated above");
        if depot.depot_from_app.is_some() {
            tracing::warn!(
                "Ignoring --manifest override for shared depot {}",
                entry.depot_id
            );
            continue;
        }
        tracing::info!(
            "Including depot {} via --manifest override despite --os filter",
            entry.depot_id
        );
        targets.push((entry.depot_id, entry.manifest_id));
    }

    Ok((targets, shared))
}

fn report_skipped_shared(skipped: &[(u32, u32)], output: &Path, platform: Platform) {
    let mut by_owner: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (depot_id, owner) in skipped {
        by_owner.entry(*owner).or_default().push(*depot_id);
    }

    for (owner, depots) in by_owner {
        let list = depots
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        tracing::warn!(
            "Shared depot(s) {} are provided by app {}. They were not downloaded.\n     Install them with:\n       steam-vent-cdn -o {} app {} --layout steam{}\n     or re-run this command with --shared-depots auto",
            list,
            owner,
            output.display(),
            owner,
            platform_arg(platform)
        );
    }
}

fn platform_arg(platform: Platform) -> String {
    match platform {
        Platform::Windows => " --os windows".to_string(),
        Platform::Macos => " --os macos".to_string(),
        Platform::Linux => " --os linux".to_string(),
        Platform::All => " --os all".to_string(),
    }
}

async fn resolve_version(
    downloader: &CdnDownloader,
    item_id: u64,
    timestamp: Option<u32>,
    manifest_id: Option<u64>,
) -> Result<VersionInfo> {
    if let Some(mid) = manifest_id {
        let mut stream = Box::pin(downloader.fetch_versions(item_id, None));
        while let Some(version) = stream.try_next().await? {
            if version.manifest_id == mid {
                return Ok(version);
            }
        }
        Err(WorkshopError::VersionNotFound(format!(
            "Manifest ID {}",
            mid
        )))?
    } else if let Some(ts) = timestamp {
        let mut stream = Box::pin(downloader.fetch_versions(item_id, None));
        while let Some(version) = stream.try_next().await? {
            if version.timestamp == ts {
                return Ok(version);
            }
        }
        Err(WorkshopError::VersionNotFound(format!("Timestamp {}", ts)))?
    } else {
        Ok(Box::pin(downloader.fetch_versions(item_id, Some(1)))
            .try_next()
            .await?
            .ok_or(WorkshopError::NoVersionsFound)?)
    }
}

fn extract_filename(details: &ItemDetails, file_url: &str) -> String {
    if let Some(name) = details.filename.as_deref().filter(|s| !s.is_empty()) {
        return name.to_string();
    }
    Url::parse(file_url)
        .ok()
        .and_then(|u| {
            u.path_segments()?
                .next_back()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "workshop_item".to_string())
}

async fn download_web_item(
    downloader: &CdnDownloader,
    file_url: &str,
    filename: &str,
    base_path: &std::path::Path,
) -> Result<()> {
    let output_path = base_path.join(filename);
    tracing::info!("Downloading web file from {}", file_url);
    let data: Vec<u8> = downloader.download_web_file(file_url).await?;
    let size = data.len() as u64;
    tokio::fs::create_dir_all(output_path.parent().unwrap_or(&output_path)).await?;
    tokio::fs::write(&output_path, &data).await?;
    tracing::info!("Downloaded web file ({} bytes) to {:?}", size, output_path);
    Ok(())
}

async fn download_cdn_files(
    manifest: &Manifest,
    base_path: &std::path::Path,
    downloader: &CdnDownloader,
    multi: &MultiProgress,
    dedup: Option<(String, u32, WrittenMap)>,
) -> Result<()> {
    for dir in &manifest.directories {
        let output_path = base_path.join(&dir.filename);
        tokio::fs::create_dir_all(&output_path).await?;
    }

    let progress = ProgressManager::new(multi, &manifest.files);
    let progress = Arc::new(progress);

    let pipeline = downloader.create_pipeline(progress.clone());

    let mut tasks = tokio::task::JoinSet::new();
    for file in &manifest.files {
        let pipeline = pipeline.clone();
        let base = base_path.to_owned();
        let file = file.clone();
        let dedup = dedup.clone();
        tasks.spawn(async move {
            if let Some((scope, depot_id, written)) = &dedup {
                let key = (scope.clone(), file.filename.clone());
                let mut paths = written
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(previous) = paths.insert(key, *depot_id)
                    && previous != *depot_id
                {
                    tracing::warn!(
                        "{} is provided by depots {} and {}; keeping depot {} (mount order)",
                        file.filename,
                        previous,
                        depot_id,
                        depot_id
                    );
                }
            }

            let mut stream = pipeline.download_file(&file)?;
            writer::write_file(&mut stream, &base).await
        });
    }

    while let Some(result) = tasks.join_next().await {
        result
            .map_err(|e| std::io::Error::other(format!("File download task panicked: {}", e)))??;
    }

    progress.finish();
    Ok(())
}

fn manifest_size(manifest: &Manifest) -> u64 {
    manifest
        .files
        .iter()
        .map(|file| match file.size {
            Some(size) => size,
            None => file
                .chunks
                .iter()
                .map(|chunk| u64::from(chunk.uncompressed_size))
                .sum(),
        })
        .sum()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use steam_vent_cdn::downloader::{AppDepot, AppInfo};

    fn sample_info() -> AppInfo {
        AppInfo {
            app_id: 440,
            name: "App".to_string(),
            build_id: 100,
            build_time: None,
            installdir: Some("App".to_string()),
            depots: vec![
                AppDepot {
                    depot_id: 441,
                    manifest_id: 555,
                    size: 10,
                    oslist: None,
                    dlc_app_id: None,
                    shared_install: false,
                    depot_from_app: None,
                },
                AppDepot {
                    depot_id: 442,
                    manifest_id: 556,
                    size: 10,
                    oslist: Some("linux".to_string()),
                    dlc_app_id: None,
                    shared_install: false,
                    depot_from_app: None,
                },
            ],
        }
    }

    #[test]
    fn rejects_unknown_manifest_override() {
        let info = sample_info();
        let err = build_targets(
            440,
            &info,
            Some("linux"),
            &[DepotManifest {
                depot_id: 999,
                manifest_id: 1,
            }],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            steam_vent_cdn::error::SteamVentError::AppInfo(AppError::UnknownDepot {
                app_id: 440,
                depot_id: 999
            })
        ));
    }

    #[test]
    fn rejects_duplicate_manifest_override() {
        let info = sample_info();
        let entry = DepotManifest {
            depot_id: 441,
            manifest_id: 1,
        };
        let err = build_targets(440, &info, Some("linux"), &[entry, entry]).unwrap_err();
        assert!(matches!(
            err,
            steam_vent_cdn::error::SteamVentError::AppInfo(AppError::DuplicateManifestOverride(
                441
            ))
        ));
    }

    #[test]
    fn manifest_override_forces_os_filtered_depot() {
        let info = sample_info();
        let (targets, _) = build_targets(
            440,
            &info,
            Some("windows"),
            &[DepotManifest {
                depot_id: 442,
                manifest_id: 777,
            }],
        )
        .unwrap();
        assert!(targets.contains(&(441, 555)));
        assert!(targets.contains(&(442, 777)));
    }
}
