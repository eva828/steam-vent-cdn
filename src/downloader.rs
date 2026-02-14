use aes::Aes256;
use aes::cipher::{BlockDecrypt, BlockDecryptMut, KeyInit, KeyIvInit, block_padding::Pkcs7};
use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose};
use cbc::Decryptor;
use reqwest::Client as HttpClient;
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;
use steam_vent::ConnectionTrait;
use steam_vent::ServerList;
use steam_vent::connection::Connection;

use steam_vent_proto::content_manifest::{ContentManifestMetadata, ContentManifestPayload};
use steam_vent_proto::protobuf::Message;
use steam_vent_proto::steammessages_clientserver_2::{
    CMsgClientGetDepotDecryptionKey, CMsgClientGetDepotDecryptionKeyResponse,
};
use steam_vent_proto::steammessages_contentsystem_steamclient::{
    CContentServerDirectory_GetManifestRequestCode_Request,
    CContentServerDirectory_GetServersForSteamPipe_Request,
};
use steam_vent_proto::steammessages_publishedfile_steamclient::CPublishedFile_GetDetails_Request;

pub struct TModDownloader {
    connection: Option<Connection>,
    http_client: HttpClient,
    server_list: ServerList,
    cdn_host: Option<String>,
}

impl TModDownloader {
    pub async fn new(_username: Option<String>, _password: Option<String>) -> Result<Self> {
        let server_list = ServerList::discover().await?;
        Ok(Self {
            connection: None,
            http_client: HttpClient::new(),
            server_list,
            cdn_host: None,
        })
    }

    pub async fn login(&mut self) -> Result<()> {
        log::info!("Connecting anonymously to Steam...");
        let mut connection = Connection::anonymous(&self.server_list).await?;
        connection.set_timeout(Duration::from_secs(30));

        // Wait a bit to ensure session is stable
        tokio::time::sleep(Duration::from_millis(500)).await;

        self.connection = Some(connection);
        log::info!("Connected.");
        Ok(())
    }

    async fn get_connection(&mut self) -> Result<&mut Connection> {
        self.connection.as_mut().context("Not logged in")
    }

    // Extracted method to fetch and decrypt manifest
    pub async fn fetch_manifest_info(
        &mut self,
        item_id: u64,
    ) -> Result<(ContentManifestPayload, String, u32)> {
        if self.cdn_host.is_none() {
            log::info!("Finding CDN servers...");
            let srv_req = CContentServerDirectory_GetServersForSteamPipe_Request {
                cell_id: Some(0),
                max_servers: Some(5),
                ..Default::default()
            };
            let conn = self.get_connection().await?;
            let srv_resp = conn.service_method(srv_req).await?;
            if srv_resp.servers.is_empty() {
                anyhow::bail!("No CDN servers available");
            }
            self.cdn_host = Some(srv_resp.servers[0].vhost().to_string());
        }
        let server = self.cdn_host.as_ref().unwrap().clone();

        let conn = self.get_connection().await?;

        log::info!("Getting item details for published file {}...", item_id);
        let details_req = CPublishedFile_GetDetails_Request {
            publishedfileids: vec![item_id],
            ..Default::default()
        };
        let details_resp = conn.service_method(details_req).await?;
        if details_resp.publishedfiledetails.is_empty() {
            anyhow::bail!("No details found for item {}", item_id);
        }
        let details = &details_resp.publishedfiledetails[0];

        let title = details.title.as_deref().unwrap_or("Unknown");
        log::info!("Title: {}", title);

        let manifest_id = details
            .hcontent_file
            .context("No manifest ID (hcontent_file) found")?;
        let app_id = details.consumer_appid.unwrap_or(1281930);
        let depot_id = app_id;

        log::info!("Manifest ID: {}", manifest_id);
        log::info!("App/Depot ID: {}", app_id);

        log::info!("Requesting manifest request code...");
        let mut code_req = CContentServerDirectory_GetManifestRequestCode_Request::new();
        code_req.set_app_id(app_id);
        code_req.set_depot_id(depot_id);
        code_req.set_manifest_id(manifest_id);
        code_req.set_app_branch("public".into());

        let code_resp = conn.service_method(code_req).await?;
        let mrc = code_resp
            .manifest_request_code
            .context("Failed to get manifest request code")?;
        log::info!("Manifest Request Code: {}", mrc);

        let manifest_url = format!(
            "https://{}/depot/{}/manifest/{}/5/{}",
            server, depot_id, manifest_id, mrc
        );
        log::info!("Downloading manifest from {}...", manifest_url);
        let resp = self.http_client.get(&manifest_url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("Manifest download failed: {}", resp.status());
        }
        let manifest_bytes = resp.bytes().await?;

        let payload_bytes = if manifest_bytes.starts_with(b"PK") {
            log::info!("Unzipping manifest...");
            let mut zip = zip::ZipArchive::new(Cursor::new(&manifest_bytes))?;
            let mut file = zip.by_index(0)?;
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer)?;
            buffer
        } else {
            manifest_bytes.to_vec()
        };

        // Detect and handle Steam manifest segments
        let mut final_payload = Vec::new();
        let mut metadata: Option<ContentManifestMetadata> = None;
        let mut cursor = 0;
        while cursor + 8 <= payload_bytes.len() {
            let magic = u32::from_le_bytes(payload_bytes[cursor..cursor + 4].try_into().unwrap());
            let length =
                u32::from_le_bytes(payload_bytes[cursor + 4..cursor + 8].try_into().unwrap())
                    as usize;

            log::info!(
                "Found manifest segment: Magic 0x{:08X}, Length {}",
                magic,
                length
            );

            if cursor + 8 + length > payload_bytes.len() {
                log::warn!(
                    "Segment length {} exceeds remaining data {}",
                    length,
                    payload_bytes.len() - cursor - 8
                );
                break;
            }

            let segment_data = &payload_bytes[cursor + 8..cursor + 8 + length];

            if magic == 0x71F617D0 {
                log::info!("Processing Payload segment...");
                final_payload.extend_from_slice(segment_data);
            } else if magic == 0x1F448235 || magic == 0x1F4812BE {
                log::info!("Decoding Metadata segment...");
                match ContentManifestMetadata::parse_from_bytes(segment_data) {
                    Ok(m) => {
                        log::info!(
                            "Metadata: Names encrypted: {:?}, Depot ID: {:?}",
                            m.filenames_encrypted,
                            m.depot_id
                        );
                        metadata = Some(m);
                    }
                    Err(e) => log::warn!("Failed to decode Metadata segment: {}", e),
                }
            } else if magic == 0x1A670D5A || magic == 0x1B81B817 {
                log::info!("Found Signature segment (skipping)");
            } else {
                log::warn!("Unknown segment magic: 0x{:08X}", magic);
            }

            cursor += 8 + length;
        }

        if final_payload.is_empty() {
            log::warn!("No Payload segment found, using raw bytes as fallback");
            final_payload = payload_bytes;
        }

        let mut manifest = ContentManifestPayload::parse_from_bytes(&final_payload)
            .context("Failed to decode manifest Protobuf")?;

        if let Some(ref m) = metadata {
            if m.filenames_encrypted.unwrap_or(false) {
                log::info!("Manifest filenames are encrypted. Requesting depot key...");
                let depot_id = m.depot_id.unwrap_or(app_id as u32);
                let conn = self.get_connection().await?;
                let mut key_req = CMsgClientGetDepotDecryptionKey::new();
                key_req.set_depot_id(depot_id);
                key_req.set_app_id(app_id);

                let key_resp: CMsgClientGetDepotDecryptionKeyResponse = conn.job(key_req).await?;
                if key_resp.eresult() != 1 {
                    anyhow::bail!(
                        "Failed to get depot decryption key: EResult {}",
                        key_resp.eresult()
                    );
                }
                let depot_key = key_resp.depot_encryption_key().to_vec();
                log::info!(
                    "Depot key received ({} bytes). Decrypting filenames...",
                    depot_key.len()
                );

                if depot_key.len() != 32 {
                    anyhow::bail!("Invalid depot key length: {}", depot_key.len());
                }

                let aes_key = aes::cipher::generic_array::GenericArray::from_slice(&depot_key);
                let cipher_ecb = Aes256::new(&aes_key);

                for mapping in &mut manifest.mappings {
                    // Decrypt filename
                    let mut new_filename: Option<String> = None;
                    if let Some(enc_val) = mapping.filename.as_ref() {
                        if !enc_val.is_empty() {
                            let enc_val_clean = enc_val.trim();
                            let decoded_res = general_purpose::STANDARD
                                .decode(enc_val_clean)
                                .or_else(|_| general_purpose::URL_SAFE.decode(enc_val_clean));

                            match decoded_res {
                                Ok(encrypted_bytes) => {
                                    if encrypted_bytes.len() >= 16 {
                                        let mut iv_bytes = [0u8; 16];
                                        iv_bytes.copy_from_slice(&encrypted_bytes[..16]);
                                        let mut iv_block =
                                            aes::cipher::generic_array::GenericArray::from_mut_slice(
                                                &mut iv_bytes,
                                            );
                                        cipher_ecb.decrypt_block(&mut iv_block);

                                        let encrypted_data = &encrypted_bytes[16..];
                                        if !encrypted_data.is_empty() {
                                            let mut buf = encrypted_data.to_vec();
                                            let decryptor = Decryptor::<Aes256>::new(
                                                &aes_key,
                                                &iv_bytes.into(),
                                            );
                                            match decryptor.decrypt_padded_mut::<Pkcs7>(&mut buf) {
                                                Ok(plaintext) => {
                                                    let name = String::from_utf8_lossy(plaintext)
                                                        .trim_matches(char::from(0))
                                                        .to_string();
                                                    log::debug!(
                                                        "Decrypted filename: {} -> {}",
                                                        enc_val,
                                                        name
                                                    );
                                                    new_filename = Some(name);
                                                }
                                                Err(e) => {
                                                    log::warn!(
                                                        "Decryption failed for {}: {:?}",
                                                        enc_val,
                                                        e
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    log::warn!("Failed to decode Base64 for {}: {:?}", enc_val, e);
                                }
                            }
                        }
                    }
                    if let Some(name) = new_filename {
                        mapping.filename = Some(name);
                    }

                    // Decrypt linktarget
                    let mut new_linktarget: Option<String> = None;
                    if let Some(enc_val) = mapping.linktarget.as_ref() {
                        if !enc_val.is_empty() {
                            let enc_val_clean = enc_val.trim();
                            let decoded_res = general_purpose::STANDARD
                                .decode(enc_val_clean)
                                .or_else(|_| general_purpose::URL_SAFE.decode(enc_val_clean));

                            match decoded_res {
                                Ok(encrypted_bytes) => {
                                    if encrypted_bytes.len() >= 16 {
                                        let mut iv_bytes = [0u8; 16];
                                        iv_bytes.copy_from_slice(&encrypted_bytes[..16]);
                                        let mut iv_block =
                                            aes::cipher::generic_array::GenericArray::from_mut_slice(
                                                &mut iv_bytes,
                                            );
                                        cipher_ecb.decrypt_block(&mut iv_block);

                                        let encrypted_data = &encrypted_bytes[16..];
                                        if !encrypted_data.is_empty() {
                                            let mut buf = encrypted_data.to_vec();
                                            let decryptor = Decryptor::<Aes256>::new(
                                                &aes_key,
                                                &iv_bytes.into(),
                                            );
                                            if let Ok(plaintext) =
                                                decryptor.decrypt_padded_mut::<Pkcs7>(&mut buf)
                                            {
                                                let name = String::from_utf8_lossy(plaintext)
                                                    .trim_matches(char::from(0))
                                                    .to_string();
                                                log::debug!(
                                                    "Decrypted linktarget: {} -> {}",
                                                    enc_val,
                                                    name
                                                );
                                                new_linktarget = Some(name);
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    log::warn!("Failed to decode Base64 for {}: {:?}", enc_val, e);
                                }
                            }
                        }
                    }
                    if let Some(target) = new_linktarget {
                        mapping.linktarget = Some(target);
                    }
                }
                log::info!("Decryption complete.");
            }
        }

        Ok((manifest, server, depot_id))
    }

    pub async fn download_item(
        &mut self,
        item_id: u64,
        output_dir: &Path,
        filter: &str,
    ) -> Result<Vec<PathBuf>> {
        let (manifest, server, depot_id) = self.fetch_manifest_info(item_id).await?;

        let mut downloaded_files = Vec::new();

        log::info!("Found {} files in manifest.", manifest.mappings.len());
        for mapping in manifest.mappings {
            let filename = mapping.filename.unwrap_or_default();
            log::info!("  File entry: {}", filename);
            if !filter.is_empty() && !filename.contains(filter) {
                continue;
            }

            log::info!(
                "Downloading {} ({} bytes)...",
                filename,
                mapping.size.unwrap_or(0)
            );
            let mut file_content = Vec::new();

            for chunk in mapping.chunks {
                let chunk_id = hex::encode(chunk.sha.unwrap_or_default());
                let chunk_url = format!("https://{}/depot/{}/chunk/{}", server, depot_id, chunk_id);

                let mut retry_count = 0;
                let chunk_payload = loop {
                    let c_resp = self.http_client.get(&chunk_url).send().await?;
                    if c_resp.status().is_success() {
                        let data = c_resp.bytes().await?;

                        if data.starts_with(b"PK") {
                            let mut zip = zip::ZipArchive::new(Cursor::new(&data))?;
                            let mut file = zip.by_index(0)?;
                            let mut buffer = Vec::new();
                            file.read_to_end(&mut buffer)?;
                            break buffer;
                        } else {
                            break data.to_vec();
                        }
                    } else if retry_count < 3 {
                        log::warn!("Retrying chunk {} (status: {})", chunk_id, c_resp.status());
                        retry_count += 1;
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    } else {
                        anyhow::bail!("Failed to download chunk {}: {}", chunk_id, c_resp.status());
                    }
                };
                file_content.extend_from_slice(&chunk_payload);
            }

            let out_path = output_dir.join(&filename);
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&out_path, &file_content)?;
            downloaded_files.push(out_path);
        }

        Ok(downloaded_files)
    }

    pub async fn list_files(&mut self, item_id: u64, filter: &str) -> Result<()> {
        let (manifest, _, _) = self.fetch_manifest_info(item_id).await?;

        println!("Listing files for item {}:", item_id);
        println!("----------------------------------------");
        for mapping in manifest.mappings {
            let filename = mapping.filename.unwrap_or_default();
            if !filter.is_empty() && !filename.contains(filter) {
                continue;
            }
            let size = mapping.size.unwrap_or(0);
            println!("{:<60} | {:>10} bytes", filename, size);
        }
        println!("----------------------------------------");

        Ok(())
    }
}
