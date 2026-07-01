use crate::downloader::archive::{ArchiveExtractor, ZipArchiveExtractor};
use crate::downloader::cdn::DepotContext;
use crate::downloader::client::CdnDownloader;
use crate::downloader::item::{ChunkInfo, DownloadItem};
use crate::error::{
    CdnError, EncryptionError, ManifestError, Result, SteamVentError, WorkshopError,
};
use aes::Aes256;
use aes::cipher::{BlockDecrypt, BlockDecryptMut, Key, KeyInit, KeyIvInit, block_padding::Pkcs7};
use base64::{Engine as _, engine::general_purpose};
use cbc::Decryptor;
use std::sync::Arc;
use steam_vent_proto::content_manifest::{ContentManifestMetadata, ContentManifestPayload};
use steam_vent_proto::protobuf::Message;

const MAGIC_PAYLOAD: u32 = 0x71F617D0;
const MAGIC_METADATA_1: u32 = 0x1F448235;
const MAGIC_METADATA_2: u32 = 0x1F4812BE;
const MAGIC_SIGNATURE_1: u32 = 0x1A670D5A;
const MAGIC_SIGNATURE_2: u32 = 0x1B81B817;

pub(crate) struct ManifestCipher {
    aes_key: Key<Aes256>,
    cipher_ecb: Aes256,
}

impl ManifestCipher {
    pub(crate) fn new(depot_key: &[u8]) -> Result<Self> {
        let cipher_ecb = Aes256::new_from_slice(depot_key)
            .map_err(|e| SteamVentError::Encryption(EncryptionError::KeyInit(e)))?;
        let aes_key = Key::<Aes256>::clone_from_slice(depot_key);

        Ok(Self {
            aes_key,
            cipher_ecb,
        })
    }

    fn decrypt_string(&self, enc_val: &str) -> Result<String> {
        if enc_val.is_empty() {
            return Err(SteamVentError::Encryption(EncryptionError::Decrypt(
                "Empty encrypted value".to_string(),
            )));
        }
        let enc_val_clean = enc_val.replace(['\n', '\r', ' '], "");
        let decoded_res = general_purpose::STANDARD
            .decode(&enc_val_clean)
            .or_else(|_| general_purpose::URL_SAFE.decode(&enc_val_clean));

        let encrypted_bytes = decoded_res.map_err(|e| {
            SteamVentError::Encryption(EncryptionError::Decrypt(format!(
                "Base64 decode failed for {}: {}",
                enc_val, e
            )))
        })?;

        if encrypted_bytes.len() < 16 {
            return Err(SteamVentError::Encryption(EncryptionError::Decrypt(
                format!(
                    "Encrypted value too short for {}: {} bytes",
                    enc_val,
                    encrypted_bytes.len()
                ),
            )));
        }

        let mut iv_bytes = [0u8; 16];
        iv_bytes.copy_from_slice(&encrypted_bytes[..16]);
        let iv_block = aes::cipher::generic_array::GenericArray::from_mut_slice(&mut iv_bytes);
        self.cipher_ecb.decrypt_block(iv_block);

        let encrypted_data = &encrypted_bytes[16..];
        if encrypted_data.is_empty() {
            return Err(SteamVentError::Encryption(EncryptionError::Decrypt(
                format!("Encrypted data empty for {}", enc_val),
            )));
        }

        let mut buf = encrypted_data.to_vec();
        let decryptor = Decryptor::<Aes256>::new(&self.aes_key, (&iv_bytes).into());
        let plaintext = decryptor
            .decrypt_padded_mut::<Pkcs7>(&mut buf)
            .map_err(|e| {
                SteamVentError::Encryption(EncryptionError::Decrypt(format!(
                    "CBC decrypt failed for {}: {}",
                    enc_val, e
                )))
            })?;

        let clean_bytes = match plaintext.iter().position(|&b| b == 0) {
            Some(pos) => &plaintext[..pos],
            None => plaintext,
        };

        Ok(String::from_utf8_lossy(clean_bytes).into_owned())
    }

    pub(crate) fn decrypt_filenames(&self, manifest: &mut ContentManifestPayload) -> Result<()> {
        for mapping in &mut manifest.mappings {
            if let Some(enc_val) = mapping.filename.as_ref() {
                let decrypted = self.decrypt_string(enc_val)?;
                tracing::debug!("Decrypted filename: {} -> {}", enc_val, decrypted);
                mapping.filename = Some(decrypted);
            }

            if let Some(enc_val) = mapping.linktarget.as_ref() {
                let decrypted = self.decrypt_string(enc_val)?;
                tracing::debug!("Decrypted linktarget: {} -> {}", enc_val, decrypted);
                mapping.linktarget = Some(decrypted);
            }
        }
        tracing::debug!("Decryption complete.");
        Ok(())
    }
}

pub(crate) fn parse_manifest(
    payload_bytes: &[u8],
) -> Result<(ContentManifestPayload, Option<ContentManifestMetadata>)> {
    let payload_bytes_vec;
    let payload_bytes = if let Ok(extractor) = ZipArchiveExtractor::new(payload_bytes) {
        payload_bytes_vec = extractor.extract()?;
        &payload_bytes_vec
    } else {
        payload_bytes
    };

    let mut final_payload = Vec::new();
    let mut metadata: Option<ContentManifestMetadata> = None;
    let mut cursor = 0;

    while cursor + 8 <= payload_bytes.len() {
        let magic = u32::from_le_bytes(payload_bytes[cursor..cursor + 4].try_into().unwrap());
        let length =
            u32::from_le_bytes(payload_bytes[cursor + 4..cursor + 8].try_into().unwrap()) as usize;

        tracing::debug!(
            "Found manifest segment: Magic 0x{:08X}, Length {}",
            magic,
            length
        );

        if cursor + 8 + length > payload_bytes.len() {
            tracing::error!(
                "Segment length {} exceeds remaining data {}",
                length,
                payload_bytes.len() - cursor - 8
            );
            return Err(SteamVentError::Manifest(
                ManifestError::SegmentLengthExceeded {
                    length,
                    remaining: payload_bytes.len() - cursor - 8,
                },
            ));
        }

        let segment_data = &payload_bytes[cursor + 8..cursor + 8 + length];

        if magic == MAGIC_PAYLOAD {
            tracing::debug!("Processing Payload segment");
            final_payload.extend_from_slice(segment_data);
        } else if magic == MAGIC_METADATA_1 || magic == MAGIC_METADATA_2 {
            tracing::debug!("Decoding Metadata segment");
            let m = ContentManifestMetadata::parse_from_bytes(segment_data)
                .map_err(|e| SteamVentError::Manifest(ManifestError::Decode(e.to_string())))?;
            tracing::debug!(
                "Metadata: Names encrypted: {:?}, Depot ID: {:?}",
                m.filenames_encrypted,
                m.depot_id
            );
            metadata = Some(m);
        } else if magic == MAGIC_SIGNATURE_1 || magic == MAGIC_SIGNATURE_2 {
            tracing::debug!("Found Signature segment (skipping)");
        } else {
            tracing::error!("Unknown segment magic: 0x{:08X}", magic);
            return Err(SteamVentError::Manifest(
                ManifestError::UnknownSegmentMagic(magic),
            ));
        }

        cursor += 8 + length;
    }

    if final_payload.is_empty() {
        tracing::error!("No Payload segment found, manifest is invalid");
        return Err(SteamVentError::Manifest(ManifestError::MissingPayload));
    }

    let manifest = ContentManifestPayload::parse_from_bytes(&final_payload)
        .map_err(|e| SteamVentError::Manifest(ManifestError::Decode(e.to_string())))?;

    Ok((manifest, metadata))
}

fn manifest_to_download_items(
    manifest: ContentManifestPayload,
    depot: Arc<DepotContext>,
) -> Result<Vec<DownloadItem>> {
    let mut items = Vec::new();

    for mut mapping in manifest.mappings {
        let filename = mapping.filename.take().ok_or_else(|| {
            SteamVentError::Manifest(ManifestError::MissingField("mapping.filename"))
        })?;
        let filename = filename.replace('\\', "/");
        let chunks: Vec<ChunkInfo> = mapping
            .chunks
            .into_iter()
            .map(|mut c| -> Result<ChunkInfo> {
                let sha = c.sha.take().ok_or_else(|| {
                    SteamVentError::Manifest(ManifestError::MissingField("chunk.sha"))
                })?;

                let crc = c.crc.take().ok_or_else(|| {
                    SteamVentError::Manifest(ManifestError::MissingField("chunk.crc"))
                })?;

                let offset = c.offset.take().ok_or_else(|| {
                    SteamVentError::Manifest(ManifestError::MissingField("chunk.offset"))
                })?;

                let uncompressed_size = c.cb_original.take().ok_or_else(|| {
                    SteamVentError::Manifest(ManifestError::MissingField("chunk.cb_original"))
                })?;

                let compressed_size = c.cb_compressed.take().ok_or_else(|| {
                    SteamVentError::Manifest(ManifestError::MissingField("chunk.cb_compressed"))
                })?;

                Ok(ChunkInfo {
                    sha,
                    crc,
                    offset,
                    uncompressed_size,
                    compressed_size,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        items.push(DownloadItem {
            filename,
            chunks,
            flags: mapping.flags.take(),
            size: mapping.size.take(),
            depot: Arc::clone(&depot),
        });
    }

    Ok(items)
}

impl CdnDownloader {
    pub async fn fetch_manifest_by_id(
        &mut self,
        manifest_id: u64,
        app_id: u32,
    ) -> Result<Vec<DownloadItem>> {
        self.cdn_servers.resolve(&mut self.steam_session).await?;
        if self.cdn_servers.is_empty() {
            return Err(SteamVentError::Cdn(CdnError::HostResolutionFailed));
        }

        let depot_id = app_id;

        tracing::debug!("Manifest ID: {}", manifest_id);
        tracing::debug!("App/Depot ID: {}", app_id);

        let mrc = self
            .fetch_manifest_request_code(app_id, depot_id, manifest_id)
            .await?;
        tracing::debug!("Manifest Request Code: {}", mrc);

        let manifest_bytes = self
            .http_fetcher
            .download_manifest(&self.cdn_servers, depot_id, manifest_id, mrc)
            .await?;

        let (mut manifest, metadata) = parse_manifest(&manifest_bytes)?;

        let mut depot_key_opt = None;
        let mut need_key_for_filenames = false;
        let mut depot_id_for_key = app_id;

        if let Some(ref m) = metadata {
            let filenames_encrypted = m.filenames_encrypted.ok_or_else(|| {
                SteamVentError::Manifest(crate::error::ManifestError::MissingField(
                    "metadata.filenames_encrypted",
                ))
            })?;
            if filenames_encrypted {
                need_key_for_filenames = true;
                depot_id_for_key = m.depot_id.ok_or_else(|| {
                    SteamVentError::Manifest(crate::error::ManifestError::MissingField(
                        "metadata.depot_id",
                    ))
                })?;
                tracing::debug!(
                    "Manifest filenames are encrypted (depot_id: {}).",
                    depot_id_for_key
                );
            }
        }

        match self.fetch_depot_key(app_id, depot_id_for_key).await {
            Ok(depot_key) => {
                tracing::debug!("Depot key received ({} bytes).", depot_key.len());
                if need_key_for_filenames {
                    let cipher = ManifestCipher::new(&depot_key)?;
                    cipher.decrypt_filenames(&mut manifest)?;
                    tracing::debug!("Decrypted manifest filenames.");
                }
                depot_key_opt = Some(depot_key);
            }
            Err(e) => {
                if need_key_for_filenames {
                    tracing::error!("Failed to fetch depot key for filename decryption: {}", e);
                    return Err(e);
                } else {
                    tracing::debug!("No depot key fetched (not needed).");
                }
            }
        }

        let depot = Arc::new(DepotContext {
            server_hosts: self.cdn_servers.clone(),
            depot_id,
            depot_key: depot_key_opt,
        });

        manifest_to_download_items(manifest, depot)
    }

    pub async fn fetch_latest_manifest_id(&mut self, item_id: u64) -> Result<u64> {
        let details = self.fetch_item_details(item_id).await?;
        details
            .manifest_id
            .ok_or_else(|| SteamVentError::Workshop(WorkshopError::MissingManifestId))
    }

    pub async fn fetch_manifest_info(
        &mut self,
        item_id: u64,
        manifest_id: u64,
    ) -> Result<Vec<DownloadItem>> {
        let details = self.fetch_item_details(item_id).await?;

        tracing::info!("Title: {}", details.title);

        let app_id = details
            .consumer_appid
            .ok_or_else(|| SteamVentError::Workshop(WorkshopError::MissingConsumerAppId))?;

        self.fetch_manifest_by_id(manifest_id, app_id).await
    }
}
