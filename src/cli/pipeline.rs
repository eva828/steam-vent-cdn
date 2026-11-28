use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::stream::TryStreamExt;
use indicatif::MultiProgress;
use url::Url;

use steam_vent_cdn::acf::{AppManifest, InstalledDepot, WorkshopItem, WorkshopManifest};
use steam_vent_cdn::downloader::client::CdnDownloader;
use steam_vent_cdn::downloader::{
    AppDepot, AppInfo, ChunkInfo, ItemDetails, Manifest, VersionInfo,
};
use steam_vent_cdn::error::{AppError, ManifestError, Result, WorkshopError};
use steam_vent_cdn::path_safety::is_relative;

use super::args::{DepotManifest, Layout, Platform, SharedDepotsMode};
use super::progress::ProgressManager;
use super::{ui, writer};

/// Depots of apps reached through shared installs are resolved layer by layer;
/// this bounds the walk in case appinfo contains a cycle.
const MAX_SHARED_DEPOT_DEPTH: u32 = 16;

/// Files written by this run, used to report overlay conflicts between depots.
#[derive(Default)]
struct WrittenFiles {
    /// File name -> depot that wrote it last (later depots win).
    owners: HashMap<String, u32>,
    /// File name -> every depot that provided it.
    conflicts: BTreeMap<String, BTreeSet<u32>>,
}

impl WrittenFiles {
    fn record(&mut self, filename: &str, depot_id: u32) {
        if let Some(previous) = self.owners.insert(filename.to_string(), depot_id)
            && previous != depot_id
        {
            self.conflicts
                .entry(filename.to_string())
                .or_default()
                .extend([previous, depot_id]);
        }
    }

    /// Number of conflicting files plus one example and its winning depot.
    fn take_summary(&mut self) -> Option<(usize, String, u32)> {
        if self.conflicts.is_empty() {
            return None;
        }

        let conflicts = std::mem::take(&mut self.conflicts);
        let (filename, winner) = conflicts
            .keys()
            .next()
            .and_then(|filename| self.owners.get(filename).map(|owner| (filename, *owner)))?;
        Some((conflicts.len(), filename.clone(), winner))
    }
}

type Overlay = Arc<Mutex<WrittenFiles>>;

type DepotTargets = (Vec<(u32, u64)>, Vec<AppDepot>);

/// What to download for one app of the shared depot graph.
struct QueuedApp {
    app_id: u32,
    /// Depots another app links to; `None` installs every selected depot.
    needed: Option<BTreeSet<u32>>,
    overrides: Vec<DepotManifest>,
}

/// Manifest data accumulated over every visit of the app during one run.
///
/// An app can be visited more than once when several owners link different
/// depots of it, so the data is merged before the manifest is written.
#[derive(Default)]
struct PendingManifest {
    name: String,
    installdir: String,
    build_id: u64,
    last_updated: u64,
    installed_depots: BTreeMap<u32, InstalledDepot>,
    shared_depots: BTreeSet<(u32, u32)>,
    bytes_to_download: u64,
    /// The app was visited as the requested app, so every selected depot is installed.
    full_selection: bool,
    /// A shared depot of the app is not installed by this run.
    shared_pending: bool,
}

impl PendingManifest {
    fn add_download(&mut self, depots: Vec<InstalledDepot>, bytes: u64) {
        for depot in depots {
            self.installed_depots.insert(depot.depot_id, depot);
        }
        self.bytes_to_download += bytes;
    }

    fn add_shared(&mut self, shared: &[AppDepot]) {
        for depot in shared {
            if let Some(owner) = depot.depot_from_app {
                self.shared_depots.insert((depot.depot_id, owner));
            }
        }
    }

    fn into_manifest(self, app_id: u32, last_owner: u64) -> AppManifest {
        AppManifest {
            app_id,
            name: self.name,
            installdir: self.installdir,
            build_id: self.build_id,
            last_updated: self.last_updated,
            bytes_to_download: self.bytes_to_download,
            last_owner,
            installed_depots: self.installed_depots.into_values().collect(),
            shared_depots: self.shared_depots.into_iter().collect(),
            complete: self.full_selection && !self.shared_pending,
        }
    }
}

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

/// Download all depots of an app for the requested platform and language.
#[allow(clippy::too_many_arguments)]
pub async fn run_app_download_pipeline(
    downloader: &CdnDownloader,
    multi: &MultiProgress,
    app_id: u32,
    output: PathBuf,
    platform: Platform,
    language: Option<&str>,
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
                language,
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
                language,
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
    language: Option<&str>,
    manifest_overrides: &[DepotManifest],
) -> Result<()> {
    let info = downloader.fetch_app_info(app_id).await?;
    tracing::info!("App '{}' (build {})", info.name, info.build_id);
    log_selection(&info, platform.oslist_name(), language);

    let (targets, shared) = build_targets(
        app_id,
        &info,
        platform.oslist_name(),
        language,
        manifest_overrides,
        None,
    )?;

    for depot in &shared {
        tracing::warn!(
            "Skipping shared depot {} (provided by app {}); use --layout steam --shared-depots auto to install it",
            depot.depot_id,
            depot.depot_from_app.unwrap_or(0)
        );
    }

    if targets.is_empty() {
        return Err(AppError::NoDepotsForSelection(app_id).into());
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
    language: Option<&str>,
    manifest_overrides: &[DepotManifest],
    shared_mode: SharedDepotsMode,
    identity: &SessionIdentity,
) -> Result<()> {
    let os = platform.oslist_name();
    let steamapps = output.join("steamapps");
    let common = steamapps.join("common");

    let mut layer = vec![QueuedApp {
        app_id,
        needed: None,
        overrides: manifest_overrides.to_vec(),
    }];
    let mut installed: HashMap<u32, BTreeSet<u32>> = HashMap::new();
    let mut manifests: BTreeMap<u32, PendingManifest> = BTreeMap::new();
    let mut depth = 0u32;

    while !layer.is_empty() {
        depth += 1;
        if depth > MAX_SHARED_DEPOT_DEPTH {
            return Err(AppError::SharedDepotDepth(MAX_SHARED_DEPOT_DEPTH).into());
        }

        let mut next: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();

        for entry in std::mem::take(&mut layer) {
            let current = entry.app_id;
            let info = downloader.fetch_app_info(current).await?;
            tracing::info!("App '{}' (build {})", info.name, info.build_id);
            if entry.needed.is_none() {
                log_selection(&info, os, language);
            }

            let (targets, shared) = build_targets(
                current,
                &info,
                os,
                language,
                &entry.overrides,
                entry.needed.as_ref(),
            )?;

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
            if !is_relative(&installdir) {
                return Err(AppError::UnsafeInstallDir {
                    app_id: current,
                    name: installdir,
                }
                .into());
            }
            let base = common.join(&installdir);
            let written: Overlay = Arc::new(Mutex::new(WrittenFiles::default()));

            let mut installed_ids = BTreeSet::new();
            let mut downloaded = Vec::new();
            let mut bytes = 0u64;
            // A re-visited owner only downloads depots it does not have yet.
            let installed_before = installed.get(&current).cloned().unwrap_or_default();

            for (depot_id, manifest_id) in targets {
                if installed_before.contains(&depot_id) {
                    continue;
                }
                tracing::info!("Downloading depot {} (manifest {})", depot_id, manifest_id);
                let manifest = downloader
                    .fetch_manifest_by_id(manifest_id, current, depot_id)
                    .await?;
                let size = manifest_size(&manifest);
                bytes += download_cdn_files(
                    &manifest,
                    &base,
                    downloader,
                    multi,
                    Some((depot_id, Arc::clone(&written))),
                )
                .await?;

                downloaded.push(InstalledDepot {
                    depot_id,
                    manifest_id,
                    size,
                    dlc_app_id: info
                        .depots
                        .iter()
                        .find(|depot| depot.depot_id == depot_id)
                        .and_then(|depot| depot.dlc_app_id),
                });
                installed_ids.insert(depot_id);
            }
            report_overlay_conflicts(current, &written);

            if downloaded.is_empty() && shared.is_empty() && installed_before.is_empty() {
                return Err(AppError::NoDepotsForSelection(current).into());
            }

            for depot in &shared {
                if let Some(owner) = depot.depot_from_app
                    && shared_mode == SharedDepotsMode::Auto
                {
                    next.entry(owner).or_default().insert(depot.depot_id);
                }
            }

            let pending = manifests.entry(current).or_insert_with(|| PendingManifest {
                name: info.name.clone(),
                installdir: installdir.clone(),
                build_id: info.build_id,
                last_updated: info.build_time.unwrap_or_else(now_secs),
                ..PendingManifest::default()
            });
            pending.add_download(downloaded, bytes);
            pending.add_shared(&shared);
            pending.full_selection |= entry.needed.is_none();
            pending.shared_pending |= shared_mode == SharedDepotsMode::Skip && !shared.is_empty();

            installed.entry(current).or_default().extend(installed_ids);
        }

        layer = next_layer(&installed, next);
    }

    // Manifests are written only after the whole shared depot walk succeeded, so
    // a failed owner download cannot leave a "fully installed" consumer behind.
    for (current, pending) in manifests {
        let acf = pending.into_manifest(current, identity.steam_id);
        acf.write(&steamapps.join(format!("appmanifest_{}.acf", current)))
            .await?;
    }

    Ok(())
}

/// Split the depots of an app into downloadable targets and shared installs.
///
/// An explicit `--manifest` override forces the depot to be included even
/// when it was filtered out by `--os` or `--language`. Unknown depot ids and
/// duplicate overrides are rejected instead of failing later with an obscure
/// CDN error. `needed` limits the result to the depots another app links to.
fn build_targets(
    app_id: u32,
    info: &AppInfo,
    os: Option<&str>,
    language: Option<&str>,
    overrides: &[DepotManifest],
    needed: Option<&BTreeSet<u32>>,
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

    if let Some(needed) = needed
        && let Some(missing) = needed
            .iter()
            .find(|id| info.depots.iter().all(|depot| depot.depot_id != **id))
    {
        return Err(AppError::SharedDepotUnavailable {
            app_id,
            depot_id: *missing,
        }
        .into());
    }

    let mut targets = Vec::new();
    let mut shared = Vec::new();

    // A linked depot is downloaded because the consuming app asked for it, not
    // because it matches this app's own platform or language metadata.
    let candidates: Vec<&AppDepot> = match needed {
        Some(needed) => info
            .depots
            .iter()
            .filter(|depot| needed.contains(&depot.depot_id))
            .collect(),
        None => info.selected_depots(os, language),
    };

    for depot in candidates {
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
        let Some(depot) = info
            .depots
            .iter()
            .find(|depot| depot.depot_id == entry.depot_id)
        else {
            return Err(AppError::UnknownDepot {
                app_id,
                depot_id: entry.depot_id,
            }
            .into());
        };
        if depot.depot_from_app.is_some() {
            tracing::warn!(
                "Ignoring --manifest override for shared depot {}",
                entry.depot_id
            );
            continue;
        }
        tracing::info!(
            "Including depot {} via --manifest override despite the filters",
            entry.depot_id
        );
        targets.push((entry.depot_id, entry.manifest_id));
    }

    Ok((targets, shared))
}

/// Owners that still have depots left to install become the next BFS layer.
fn next_layer(
    installed: &HashMap<u32, BTreeSet<u32>>,
    next: BTreeMap<u32, BTreeSet<u32>>,
) -> Vec<QueuedApp> {
    next.into_iter()
        .filter(|(owner, needed)| {
            !installed
                .get(owner)
                .is_some_and(|done| needed.is_subset(done))
        })
        .map(|(owner, needed)| QueuedApp {
            app_id: owner,
            needed: Some(needed),
            overrides: Vec::new(),
        })
        .collect()
}

fn log_selection(info: &AppInfo, os: Option<&str>, language: Option<&str>) {
    let selected: HashSet<u32> = info
        .selected_depots(os, language)
        .iter()
        .map(|depot| depot.depot_id)
        .collect();
    let excluded: Vec<u32> = info
        .depots
        .iter()
        .map(|depot| depot.depot_id)
        .filter(|depot_id| !selected.contains(depot_id))
        .collect();

    if !excluded.is_empty() {
        tracing::info!(
            "Selected {} of {} depot(s) for os={}, language={}; skipping {}",
            selected.len(),
            info.depots.len(),
            os.unwrap_or("all"),
            language.unwrap_or("all"),
            excluded
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

fn report_overlay_conflicts(app_id: u32, written: &Overlay) {
    let summary = written
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take_summary();

    if let Some((count, filename, winner)) = summary {
        tracing::warn!(
            "{} file(s) of app {} are provided by more than one depot; the last depot in appinfo order wins (e.g. '{}' from depot {})",
            count,
            app_id,
            filename,
            winner
        );
    }
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
            "Shared depot(s) {} are provided by app {} and were not downloaded.\n     Install them with:\n       steam-vent-cdn -o {} app {} --layout steam{}\n     or re-run this command with --shared-depots auto.\n     They are already recorded in the app manifest's SharedDepots.",
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
    if !is_relative(filename) {
        return Err(ManifestError::UnsafePath {
            name: filename.to_string(),
        }
        .into());
    }

    let output_path = base_path.join(filename);
    tracing::info!("Downloading web file from {}", file_url);
    let data: Vec<u8> = downloader.download_web_file(file_url).await?;
    let size = data.len() as u64;
    tokio::fs::create_dir_all(output_path.parent().unwrap_or(&output_path)).await?;
    tokio::fs::write(&output_path, &data).await?;
    tracing::info!("Downloaded web file ({} bytes) to {:?}", size, output_path);
    Ok(())
}

/// Download every file of a manifest.
///
/// Returns the compressed size declared by the manifest, which is the amount
/// that had to be transferred from the CDN.
async fn download_cdn_files(
    manifest: &Manifest,
    base_path: &std::path::Path,
    downloader: &CdnDownloader,
    multi: &MultiProgress,
    overlay: Option<(u32, Overlay)>,
) -> Result<u64> {
    for dir in &manifest.directories {
        if !is_relative(&dir.filename) {
            return Err(ManifestError::UnsafePath {
                name: dir.filename.clone(),
            }
            .into());
        }
        let output_path = base_path.join(&dir.filename);
        tokio::fs::create_dir_all(&output_path).await?;
    }

    let bytes = manifest_download_size(manifest);

    let progress = ProgressManager::new(multi, &manifest.files);
    let progress = Arc::new(progress);

    let pipeline = downloader.create_pipeline(progress.clone());

    let mut tasks = tokio::task::JoinSet::new();
    for file in &manifest.files {
        let pipeline = pipeline.clone();
        let base = base_path.to_owned();
        let file = file.clone();
        let overlay = overlay.clone();
        tasks.spawn(async move {
            if let Some((depot_id, written)) = &overlay {
                written
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .record(&file.filename, *depot_id);
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
    Ok(bytes)
}

/// Uncompressed size of the files a manifest installs.
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

/// Amount that has to be transferred for a manifest.
fn manifest_download_size(manifest: &Manifest) -> u64 {
    compressed_size(manifest.files.iter().flat_map(|file| file.chunks.iter()))
}

fn compressed_size<'a>(chunks: impl Iterator<Item = &'a ChunkInfo>) -> u64 {
    chunks.map(|chunk| u64::from(chunk.compressed_size)).sum()
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

    fn depot(depot_id: u32, manifest_id: u64) -> AppDepot {
        AppDepot {
            depot_id,
            manifest_id,
            size: 10,
            oslist: None,
            language: None,
            lowviolence: false,
            dlc_app_id: None,
            depot_from_app: None,
        }
    }

    fn sample_info() -> AppInfo {
        AppInfo {
            app_id: 440,
            name: "App".to_string(),
            build_id: 100,
            build_time: None,
            installdir: Some("App".to_string()),
            depots: vec![depot(441, 555), depot(442, 556)],
        }
    }

    #[test]
    fn rejects_unknown_manifest_override() {
        let info = sample_info();
        let err = build_targets(
            440,
            &info,
            Some("linux"),
            None,
            &[DepotManifest {
                depot_id: 999,
                manifest_id: 1,
            }],
            None,
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
        let err =
            build_targets(440, &info, Some("linux"), None, &[entry, entry], None).unwrap_err();
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
            None,
            &[DepotManifest {
                depot_id: 442,
                manifest_id: 777,
            }],
            None,
        )
        .unwrap();
        assert_eq!(targets, vec![(441, 555), (442, 777)]);
    }

    #[test]
    fn keeps_appinfo_order_for_overlays() {
        let mut info = sample_info();
        info.depots.reverse();

        let (targets, _) = build_targets(440, &info, None, None, &[], None).unwrap();
        assert_eq!(targets, vec![(442, 556), (441, 555)]);
    }

    #[test]
    fn language_filter_drops_localized_depots() {
        let mut info = sample_info();
        info.depots[0].language = Some("french".to_string());

        let (targets, _) = build_targets(440, &info, None, Some("english"), &[], None).unwrap();
        assert_eq!(targets, vec![(442, 556)]);
    }

    #[test]
    fn needed_limits_linked_depots() {
        let mut info = sample_info();
        info.depots[0].oslist = Some("windows".to_string());

        let needed = BTreeSet::from([442]);
        let (targets, shared) = build_targets(
            228980,
            &info,
            Some("linux"),
            Some("english"),
            &[],
            Some(&needed),
        )
        .unwrap();
        assert_eq!(targets, vec![(442, 556)]);
        assert!(shared.is_empty());
    }

    #[test]
    fn needed_depot_missing_from_owner_is_rejected() {
        let info = sample_info();
        let needed = BTreeSet::from([999]);

        let err = build_targets(228980, &info, None, None, &[], Some(&needed)).unwrap_err();
        assert!(matches!(
            err,
            steam_vent_cdn::error::SteamVentError::AppInfo(AppError::SharedDepotUnavailable {
                app_id: 228980,
                depot_id: 999
            })
        ));
    }

    #[test]
    fn shared_depots_are_not_targets() {
        let mut info = sample_info();
        info.depots[1].depot_from_app = Some(228980);

        let (targets, shared) = build_targets(440, &info, None, None, &[], None).unwrap();
        assert_eq!(targets, vec![(441, 555)]);
        assert_eq!(shared.len(), 1);
        assert_eq!(shared[0].depot_id, 442);
    }

    #[test]
    fn low_violence_depots_are_never_selected() {
        let mut info = sample_info();
        info.depots[0].lowviolence = true;

        let (targets, _) = build_targets(440, &info, None, None, &[], None).unwrap();
        assert_eq!(targets, vec![(442, 556)]);
    }

    #[test]
    fn overlay_tracker_reports_later_depot() {
        let mut written = WrittenFiles::default();
        written.record("bin/game.exe", 441);
        written.record("bin/game.exe", 442);
        written.record("readme.txt", 441);

        let (count, filename, winner) = written.take_summary().unwrap();
        assert_eq!(count, 1);
        assert_eq!(filename, "bin/game.exe");
        assert_eq!(winner, 442);
        assert!(written.take_summary().is_none());
    }

    fn shared_depot(depot_id: u32, owner: u32) -> AppDepot {
        let mut depot = depot(depot_id, 0);
        depot.depot_from_app = Some(owner);
        depot
    }

    #[test]
    fn next_layer_skips_installed_subsets() {
        let installed = HashMap::from([(228980, BTreeSet::from([100, 101]))]);
        let next = BTreeMap::from([
            (228980, BTreeSet::from([100])),
            (228981, BTreeSet::from([200])),
        ]);

        let layer = next_layer(&installed, next);
        assert_eq!(layer.len(), 1);
        assert_eq!(layer[0].app_id, 228981);
        assert_eq!(layer[0].needed.as_ref().unwrap(), &BTreeSet::from([200]));
    }

    #[test]
    fn next_layer_requeues_when_more_depots_are_needed() {
        let installed = HashMap::from([(228980, BTreeSet::from([100]))]);
        let next = BTreeMap::from([(228980, BTreeSet::from([100, 101]))]);

        let layer = next_layer(&installed, next);
        assert_eq!(layer.len(), 1);
        assert_eq!(layer[0].app_id, 228980);
        assert_eq!(
            layer[0].needed.as_ref().unwrap(),
            &BTreeSet::from([100, 101])
        );
    }

    #[test]
    fn pending_manifest_accumulates_repeated_visits() {
        let mut pending = PendingManifest {
            name: "Owner".to_string(),
            installdir: "Owner".to_string(),
            build_id: 1,
            last_updated: 2,
            ..PendingManifest::default()
        };

        pending.add_download(
            vec![InstalledDepot {
                depot_id: 100,
                manifest_id: 10,
                size: 5,
                dlc_app_id: None,
            }],
            7,
        );
        pending.add_download(
            vec![InstalledDepot {
                depot_id: 101,
                manifest_id: 11,
                size: 6,
                dlc_app_id: Some(9),
            }],
            8,
        );
        pending.add_shared(&[shared_depot(102, 228980)]);

        let manifest = pending.into_manifest(228980, 0);
        assert_eq!(manifest.bytes_to_download, 15);
        assert_eq!(manifest.installed_depots.len(), 2);
        assert_eq!(manifest.shared_depots, vec![(102, 228980)]);
        assert!(!manifest.complete);
    }

    #[test]
    fn fully_selected_manifest_is_complete() {
        let mut pending = PendingManifest {
            full_selection: true,
            ..PendingManifest::default()
        };
        pending.add_download(Vec::new(), 0);

        assert!(pending.into_manifest(440, 0).complete);
    }

    #[test]
    fn compressed_size_sums_chunk_sizes() {
        let chunks = [
            ChunkInfo {
                sha1: vec![0; 20],
                crc: 0,
                offset: 0,
                uncompressed_size: 100,
                compressed_size: 40,
            },
            ChunkInfo {
                sha1: vec![0; 20],
                crc: 0,
                offset: 0,
                uncompressed_size: 200,
                compressed_size: 60,
            },
        ];

        assert_eq!(compressed_size(chunks.iter()), 100);
    }
}
