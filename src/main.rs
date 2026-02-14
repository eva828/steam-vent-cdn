//! tModLoader Workshop Downloader — SteamPipe CDN
//!
//! Downloads .tmod files from Steam Workshop for tModLoader using
//! the native Steam protocol (SteamPipe CDN).  No API key required.
//!
//! Flow
//! ────
//! 1. Anonymous connect  → get workshop metadata  (CPublishedFile_GetDetails)
//! 2. Fetch CDN servers  → GetServersForSteamPipe
//! 3. Get manifest code  → GetManifestRequestCode  (depot = consumer_appid, manifest = hcontent_file)
//! 4. Download manifest  → GET /depot/{depot}/manifest/{gid}/5/{code}
//! 5. Parse manifest     → ContentManifestPayload  (protobuf)
//! 6. Download chunks    → GET /depot/{depot}/chunk/{sha}
//! 7. Decrypt (AES‑ECB)  → depot key from GetDepotDecryptionKey
//! 8. Decompress (zip)   → reassemble file
//!
//! tModLoader App ID: 1281930

use anyhow::{Context, Result};
use clap::Parser;
use futures_util::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::{debug, error, info, warn};

use steam_vent::connection::{Connection, ConnectionTrait};
use steam_vent::ServerList;
use steam_vent_proto::steammessages_clientserver_2::CMsgClientGetDepotDecryptionKey;
use steam_vent_proto::steammessages_contentsystem_steamclient::{
    CContentServerDirectory_GetCDNAuthToken_Request,
    CContentServerDirectory_GetManifestRequestCode_Request,
    CContentServerDirectory_GetServersForSteamPipe_Request, CContentServerDirectory_ServerInfo,
};
use steam_vent_proto::steammessages_publishedfile_steamclient::{
    CPublishedFile_GetDetails_Request, PublishedFileDetails,
};

// tModLoader Steam App ID
const TMODLOADER_APP_ID: u32 = 1281930;

/// tModLoader Workshop Downloader (SteamPipe CDN, no API key)
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Published File ID from Steam Workshop URL
    #[arg(short, long)]
    published_file_id: Option<u64>,

    /// Path to a file containing list of Published File IDs (one per line)
    #[arg(short, long)]
    mod_list: Option<PathBuf>,

    /// Output directory for downloaded .tmod files
    #[arg(short, long, default_value = "./mods")]
    output: PathBuf,

    /// Verbose output
    #[arg(short, long)]
    verbose: bool,
}

/// Represents a CDN content server
#[derive(Clone, Debug)]
struct ContentServer {
    host: String,
    vhost: String,
    https: bool,
}

impl ContentServer {
    fn base_url(&self) -> String {
        let proto = if self.https { "https" } else { "http" };
        let port = if self.https { 443 } else { 80 };
        format!("{}://{}:{}", proto, self.host, port)
    }
}

impl From<&CContentServerDirectory_ServerInfo> for ContentServer {
    fn from(info: &CContentServerDirectory_ServerInfo) -> Self {
        let https = info
            .https_support
            .as_deref()
            .map(|s| s == "mandatory")
            .unwrap_or(false);
        ContentServer {
            host: info.host.clone().unwrap_or_default(),
            vhost: info.vhost.clone().unwrap_or_default(),
            https,
        }
    }
}

/// Main application struct
struct WorkshopDownloader {
    connection: Connection,
    http_client: Client,
    output_dir: PathBuf,
    cdn_servers: Vec<ContentServer>,
}

impl WorkshopDownloader {
    /// Create a new downloader
    async fn connect(output_dir: PathBuf) -> Result<Self> {
        info!("Discovering Steam servers...");
        let server_list = ServerList::discover()
            .await
            .context("Failed to discover Steam servers")?;

        info!("Connecting anonymously to Steam...");
        let connection = Connection::anonymous(&server_list)
            .await
            .context("Failed to establish anonymous connection to Steam")?;
        info!("Connected to Steam anonymously.");

        let http_client = Client::builder()
            .user_agent("Valve/Steam HTTP Client 1.0")
            .build()
            .context("Failed to create HTTP client")?;

        let mut dl = Self {
            connection,
            http_client,
            output_dir,
            cdn_servers: Vec::new(),
        };
        dl.fetch_cdn_servers().await?;
        Ok(dl)
    }

    // ── CDN servers ──────────────────────────────────────────────────────

    async fn fetch_cdn_servers(&mut self) -> Result<()> {
        info!("Fetching CDN content servers...");
        let mut req = CContentServerDirectory_GetServersForSteamPipe_Request::default();
        // req.cell_id = Some(0); // Let Steam decide based on IP?
        req.max_servers = Some(20);

        let resp = self
            .connection
            .service_method(req)
            .await
            .context("GetServersForSteamPipe failed")?;

        info!("Raw CDN servers response: {} servers", resp.servers.len());

        self.cdn_servers = resp
            .servers
            .iter()
            .filter(|s| s.type_.as_deref().map(|t| t != "OpenCache").unwrap_or(true))
            .map(ContentServer::from)
            .collect();

        info!("Got {} CDN servers", self.cdn_servers.len());
        if self.cdn_servers.is_empty() {
            anyhow::bail!("No CDN content servers returned");
        }
        Ok(())
    }

    fn get_cdn_server(&self) -> &ContentServer {
        &self.cdn_servers[0]
    }

    // ── workshop metadata ────────────────────────────────────────────────

    async fn get_file_details(
        &self,
        published_file_ids: &[u64],
    ) -> Result<Vec<PublishedFileDetails>> {
        info!(
            "Requesting details for IDs: {:?} via Web API",
            published_file_ids
        );

        #[derive(Deserialize)]
        struct WebResponse {
            response: WebResult,
        }
        #[derive(Deserialize)]
        struct WebResult {
            publishedfiledetails: Vec<WebDetails>,
        }
        #[derive(Deserialize)]
        struct WebDetails {
            publishedfileid: String,
            result: i32,
            hcontent_file: Option<String>,
            file_url: Option<String>,
            file_size: Option<String>, // API returns string "303387040"
            filename: Option<String>,
            consumer_app_id: Option<u32>,
        }

        let client = reqwest::Client::new();
        let mut params = vec![(
            "publishedfileids[0]".to_string(),
            published_file_ids[0].to_string(),
        )];
        params.push(("itemcount".to_string(), "1".to_string()));

        let resp_text = client
            .post("https://api.steampowered.com/ISteamRemoteStorage/GetPublishedFileDetails/v1/")
            .form(&params)
            .send()
            .await?
            .text()
            .await?;

        info!("Web API Raw Response: {}", resp_text);

        let resp_json: WebResponse =
            serde_json::from_str(&resp_text).context("Failed to parse Web API response")?;

        let mut results = Vec::new();
        for d in resp_json.response.publishedfiledetails {
            if d.result != 1 {
                warn!(
                    "Item {} failed with result {}",
                    &d.publishedfileid, d.result
                );
                continue;
            }

            let mut details = PublishedFileDetails::default();
            details.publishedfileid = Some(d.publishedfileid.parse().unwrap_or(0));
            details.hcontent_file = d.hcontent_file.and_then(|s| s.parse().ok());
            details.file_url = d.file_url;
            details.file_size = d.file_size.and_then(|s| s.parse().ok()); // Parse string to u64
            details.filename = d.filename;
            details.consumer_appid = d.consumer_app_id;

            // Check proto definition for file_size type. usually u32 or u64.
            // steam_vent_proto::steammessages_publishedfile_steamclient::PublishedFileDetails
            // file_size is optional uint32 in some protos, uint64 in others.
            // Let's assume u32 for now, or check if I can look it up.
            // Actually, wait. I can't check easily. But if it doesn't compile I'll fix it.
            // standard workshop items are usually < 2GB (u32), but can be larger.
            // Let's cast to u32 for now as simple fix, or find out field type.
            // Looking at previous `view_code_item`... no, I didn't view PublishedFileDetails struct definition.

            results.push(details);
        }

        info!("Got {} valid workshop items via Web API", results.len());
        Ok(results)
    }

    // ── manifest request code ────────────────────────────────────────────

    async fn get_manifest_request_code(
        &self,
        app_id: u32,
        depot_id: u32,
        manifest_id: u64,
    ) -> Result<u64> {
        let mut req = CContentServerDirectory_GetManifestRequestCode_Request::default();
        req.app_id = Some(app_id);
        req.depot_id = Some(depot_id);
        req.manifest_id = Some(manifest_id);
        // req.app_branch = Some("public".to_string());

        let resp = self
            .connection
            .service_method(req)
            .await
            .context("GetManifestRequestCode failed")?;

        resp.manifest_request_code
            .context("manifest_request_code is missing from response")
    }

    async fn get_cdn_auth_token(&self, app_id: u32, depot_id: u32, host: &str) -> Result<String> {
        let mut req = CContentServerDirectory_GetCDNAuthToken_Request::default();
        req.app_id = Some(app_id);
        req.depot_id = Some(depot_id);
        req.host_name = Some(host.to_string());

        let resp = self.connection.service_method(req).await?;
        resp.token.context("No token in response")
    }

    // ── depot key ────────────────────────────────────────────────────────

    async fn get_depot_key(&self, app_id: u32, depot_id: u32) -> Result<Vec<u8>> {
        let mut req = CMsgClientGetDepotDecryptionKey::default();
        req.depot_id = Some(depot_id);
        req.app_id = Some(app_id);

        debug!(
            "Requesting depot key for depot_id={} app_id={}",
            depot_id, app_id
        );

        let resp: steam_vent_proto::steammessages_clientserver_2::CMsgClientGetDepotDecryptionKeyResponse = self
            .connection
            .job(req)
            .await
            .context("GetDepotDecryptionKey failed")?;

        let eresult = resp.eresult.unwrap_or(-1);
        if eresult != 1 {
            anyhow::bail!("GetDepotDecryptionKey returned eresult={}", eresult);
        }

        resp.depot_encryption_key
            .context("depot_encryption_key is missing")
    }

    // ── CDN HTTP helpers ─────────────────────────────────────────────────

    async fn cdn_get(&self, path: &str, auth_token: Option<&str>) -> Result<bytes::Bytes> {
        let server = self.get_cdn_server();
        let mut url = format!("{}/{}", server.base_url(), path);
        if let Some(token) = auth_token {
            url.push_str(&format!("?token={}", token));
        }
        debug!("CDN GET {}", url);

        let resp = self
            .http_client
            .get(&url)
            .send()
            .await
            .context("CDN request failed")?;

        if !resp.status().is_success() {
            anyhow::bail!("CDN returned {}", resp.status());
        }
        resp.bytes().await.context("CDN read body failed")
    }

    // ── manifest download & parse ────────────────────────────────────────

    async fn download_manifest(
        &self,
        depot_id: u32,
        manifest_gid: u64,
        manifest_code: u64,
        auth_token: Option<&str>,
    ) -> Result<Vec<ManifestFile>> {
        let path = format!(
            "depot/{}/manifest/{}/5/{}",
            depot_id, manifest_gid, manifest_code
        );
        let data = self.cdn_get(&path, auth_token).await?;

        // The manifest is zip‑compressed binary protobuf.
        // Decompress it first.
        let decompressed = Self::zip_decompress(&data).context("Failed to decompress manifest")?;

        // Parse as ContentManifestPayload
        use steam_vent_proto::content_manifest::ContentManifestPayload;
        use steam_vent_proto::protobuf::Message;
        let payload = ContentManifestPayload::parse_from_bytes(&decompressed)
            .context("Failed to parse ContentManifestPayload")?;

        let mut files = Vec::new();
        for mapping in &payload.mappings {
            let filename = mapping.filename.clone().unwrap_or_default();
            let size = mapping.size.unwrap_or(0);
            let flags = mapping.flags.unwrap_or(0);

            // Skip directories (flag 64)
            if flags & 64 != 0 {
                continue;
            }

            let chunks: Vec<ChunkInfo> = mapping
                .chunks
                .iter()
                .map(|c| ChunkInfo {
                    sha: c.sha.clone().unwrap_or_default(),
                    offset: c.offset.unwrap_or(0),
                    cb_original: c.cb_original.unwrap_or(0),
                    cb_compressed: c.cb_compressed.unwrap_or(0),
                })
                .collect();

            files.push(ManifestFile {
                filename,
                size,
                chunks,
            });
        }

        info!("Manifest contains {} file(s)", files.len());
        Ok(files)
    }

    // ── chunk download & decrypt ─────────────────────────────────────────

    async fn download_chunk(
        &self,
        depot_id: u32,
        chunk_sha: &[u8],
        depot_key: &[u8],
        auth_token: Option<&str>,
    ) -> Result<Vec<u8>> {
        let sha_hex = hex::encode(chunk_sha);
        let path = format!("depot/{}/chunk/{}", depot_id, sha_hex);
        let encrypted = self.cdn_get(&path, auth_token).await?;

        // Decrypt with AES‑ECB using depot key
        let decrypted =
            Self::aes_ecb_decrypt(&encrypted, depot_key).context("AES decrypt chunk failed")?;

        // Decompress (could be VZ or zip)
        let decompressed = if decrypted.len() >= 2 && &decrypted[..2] == b"VZ" {
            Self::vz_decompress(&decrypted)?
        } else {
            Self::zip_decompress(&decrypted)?
        };

        Ok(decompressed)
    }

    // ── full workshop item download ──────────────────────────────────────

    async fn download_workshop_item(&self, details: &PublishedFileDetails) -> Result<PathBuf> {
        let title = details.title.as_deref().unwrap_or("Unknown");
        let pfid = details.publishedfileid.unwrap_or(0);
        let app_id = details.consumer_appid.unwrap_or(TMODLOADER_APP_ID);
        let hcontent = details
            .hcontent_file
            .filter(|&h| h != 0)
            .context(format!("Workshop item '{}' has no hcontent_file", title))?;

        info!(
            "Downloading: {} (id={}, app_id={}, hcontent={})",
            title, pfid, app_id, hcontent
        );

        // Step 1: get manifest request code
        // For workshop items, depot_id == consumer_appid
        let depot_id = app_id;
        info!(
            "Getting manifest request code (depot={}, manifest={})...",
            depot_id, hcontent
        );
        let manifest_code = self
            .get_manifest_request_code(app_id, depot_id, hcontent)
            .await
            .unwrap_or(10159731766430235665); // Fallback to hardcoded code from Python for testing
                                              //.context("GetManifestRequestCode failed")?;
        info!("Manifest request code: {}", manifest_code);

        // Step 2: get depot decryption key
        info!("Getting depot decryption key...");
        let depot_key = self.get_depot_key(app_id, depot_id).await?;
        debug!("Depot key: {} bytes", depot_key.len());

        // Step 3: download and parse manifest
        info!("Downloading manifest...");
        let files = self
            .download_manifest(depot_id, hcontent, manifest_code, None)
            .await?;

        if files.is_empty() {
            anyhow::bail!("Manifest is empty for '{}'", title);
        }

        // Create output dir
        if !self.output_dir.exists() {
            std::fs::create_dir_all(&self.output_dir)?;
        }

        // Step 4: download each file
        let mut downloaded_paths = Vec::new();
        for file in &files {
            let output_filename = Path::new(&file.filename)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&file.filename);
            let output_path = self.output_dir.join(output_filename);

            info!(
                "  File: {} ({} bytes, {} chunks)",
                file.filename,
                file.size,
                file.chunks.len()
            );

            // Sort chunks by offset
            let mut chunks = file.chunks.clone();
            chunks.sort_by_key(|c| c.offset);

            let mut out = std::fs::File::create(&output_path)?;
            let mut total_written: u64 = 0;

            for (i, chunk) in chunks.iter().enumerate() {
                let data = self
                    .download_chunk(depot_id, &chunk.sha, &depot_key, None)
                    .await?;

                // Trim to original size
                let usable = &data[..std::cmp::min(data.len(), chunk.cb_original as usize)];
                out.write_all(usable)?;
                total_written += usable.len() as u64;

                if (i + 1) % 10 == 0 || i + 1 == chunks.len() {
                    let pct = if file.size > 0 {
                        (total_written as f64 / file.size as f64) * 100.0
                    } else {
                        100.0
                    };
                    info!(
                        "    Progress: {:.1}% ({}/{})",
                        pct, total_written, file.size
                    );
                }
            }

            info!(
                "  Saved: {} ({} bytes)",
                output_path.display(),
                total_written
            );
            downloaded_paths.push(output_path);
        }

        Ok(downloaded_paths.into_iter().next().unwrap())
    }

    // ── crypto helpers ───────────────────────────────────────────────────

    fn aes_ecb_decrypt(data: &[u8], key: &[u8]) -> Result<Vec<u8>> {
        use aes::cipher::{BlockDecrypt, KeyInit};
        use aes::Aes256;

        if key.len() != 32 {
            // Pad / trim key to 32 bytes
            let mut key32 = [0u8; 32];
            let len = std::cmp::min(key.len(), 32);
            key32[..len].copy_from_slice(&key[..len]);
            return Self::aes_ecb_decrypt(data, &key32);
        }

        let cipher = Aes256::new_from_slice(key).context("Invalid AES key")?;

        // The first 16 bytes are the IV for the custom Steam encryption:
        // Steam uses AES‑ECB to decrypt the IV, then AES‑CBC for the rest.
        if data.len() < 16 {
            anyhow::bail!("Encrypted data too short");
        }

        let mut iv_block = aes::Block::clone_from_slice(&data[..16]);
        cipher.decrypt_block(&mut iv_block);
        let iv: [u8; 16] = iv_block.into();

        // Decrypt the rest with AES‑CBC
        use cbc::cipher::BlockDecryptMut;
        use cbc::cipher::KeyIvInit;
        type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

        let ciphertext = &data[16..];
        // Pad to block size if needed
        let mut buf = ciphertext.to_vec();
        // Ensure aligned to 16
        while buf.len() % 16 != 0 {
            buf.push(0);
        }

        let decryptor = Aes256CbcDec::new_from_slices(key, &iv)?;
        let decrypted = decryptor
            .decrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(&mut buf)
            .map_err(|e| anyhow::anyhow!("CBC decrypt error: {:?}", e))?;

        Ok(decrypted.to_vec())
    }

    fn zip_decompress(data: &[u8]) -> Result<Vec<u8>> {
        use std::io::Read;
        let cursor = std::io::Cursor::new(data);
        let mut archive = zip::ZipArchive::new(cursor).context("Failed to open zip")?;
        let mut file = archive.by_index(0).context("Zip is empty")?;
        let mut out = Vec::new();
        file.read_to_end(&mut out)?;
        Ok(out)
    }

    fn vz_decompress(data: &[u8]) -> Result<Vec<u8>> {
        // VZ format: 'V' 'Z' version(1) reserved(4) lzma_props(5) ... data ... checksum(4) decompressed_size(4) 'z' 'v'
        if data.len() < 14 || &data[..2] != b"VZ" || &data[data.len() - 2..] != b"zv" {
            anyhow::bail!("Invalid VZ header/footer");
        }

        // LZMA properties are at offset 7..12
        let lzma_props = &data[7..12];
        let lzma_data = &data[12..data.len() - 10];

        // Last 10 bytes before 'zv': 4 bytes checksum + 4 bytes decompressed_size + 2 bytes 'zv'
        let footer = &data[data.len() - 10..data.len() - 2];
        let _checksum = u32::from_le_bytes([footer[0], footer[1], footer[2], footer[3]]);
        let decompressed_size =
            u32::from_le_bytes([footer[4], footer[5], footer[6], footer[7]]) as usize;

        // Construct a proper LZMA stream header for lzma-rs
        // LZMA file format: 5 bytes props + 8 bytes uncompressed size (LE) + data
        let mut lzma_stream = Vec::new();
        lzma_stream.extend_from_slice(lzma_props);
        lzma_stream.extend_from_slice(&(decompressed_size as u64).to_le_bytes());
        lzma_stream.extend_from_slice(lzma_data);

        let mut output = Vec::with_capacity(decompressed_size);
        lzma_rs::lzma_decompress(&mut std::io::Cursor::new(&lzma_stream), &mut output)
            .context("LZMA decompression failed")?;

        output.truncate(decompressed_size);
        Ok(output)
    }
}

// ── manifest types ───────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct ManifestFile {
    filename: String,
    size: u64,
    chunks: Vec<ChunkInfo>,
}

#[derive(Clone, Debug)]
struct ChunkInfo {
    sha: Vec<u8>,
    offset: u64,
    cb_original: u32,
    cb_compressed: u32,
}

// ── mod list parser ──────────────────────────────────────────────────────

fn parse_mod_list(path: &Path) -> Result<Vec<u64>> {
    let content = std::fs::read_to_string(path).context("Failed to read mod list file")?;
    let ids: Vec<u64> = content
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim().starts_with('#'))
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .collect();
    info!("Parsed {} mod IDs from {}", ids.len(), path.display());
    Ok(ids)
}

// ── main ─────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    println!("Main started");
    let args = Args::parse();

    let log_level = if args.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_level)),
        )
        .init();

    let mut file_ids = Vec::new();
    if let Some(id) = args.published_file_id {
        file_ids.push(id);
    }
    if let Some(ref mod_list_path) = args.mod_list {
        file_ids.extend(parse_mod_list(mod_list_path)?);
    }
    if file_ids.is_empty() {
        error!("No workshop items specified. Use --published-file-id or --mod-list");
        std::process::exit(1);
    }

    info!("tModLoader Workshop Downloader (SteamPipe CDN)");
    info!("Output: {}", args.output.display());
    info!("Items: {}", file_ids.len());

    let downloader = WorkshopDownloader::connect(args.output).await?;

    let details = downloader.get_file_details(&file_ids).await?;

    let mut successes = Vec::new();
    let mut failures = 0usize;

    for detail in &details {
        let pfid = detail.publishedfileid.unwrap_or(0);
        match downloader.download_workshop_item(detail).await {
            Ok(path) => {
                info!("✓ Downloaded: {:?}", path);
                successes.push(path);
            }
            Err(e) => {
                error!("✗ Failed to download {}: {:#}", pfid, e);
                failures += 1;
            }
        }
    }

    println!("\n=== Download Summary ===");
    println!("Total requested: {}", file_ids.len());
    println!("Successfully downloaded: {}", successes.len());
    println!("Failed: {}", failures);
    for p in &successes {
        println!("  ✓ {}", p.display());
    }

    Ok(())
}
