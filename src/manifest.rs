use crate::error::{Result, SteamVentError};

use aes::Aes256;
use aes::cipher::{BlockDecrypt, BlockDecryptMut, KeyInit, KeyIvInit, block_padding::Pkcs7};
use base64::{Engine as _, engine::general_purpose};
use cbc::Decryptor;
use steam_vent_proto::content_manifest::{ContentManifestMetadata, ContentManifestPayload};
use steam_vent_proto::protobuf::Message;

pub trait ManifestParser: Send + Sync {
    fn parse(
        &self,
        payload_bytes: &[u8],
    ) -> Result<(ContentManifestPayload, Option<ContentManifestMetadata>)>;
    fn decrypt_filenames(
        &self,
        manifest: &mut ContentManifestPayload,
        depot_key: &[u8],
    ) -> Result<()>;
}

pub struct ProtobufManifestParser;

impl ProtobufManifestParser {
    fn decrypt_string(
        enc_val: &str,
        aes_key: &aes::cipher::generic_array::GenericArray<u8, aes::cipher::typenum::U32>,
        cipher_ecb: &Aes256,
    ) -> Option<String> {
        if enc_val.is_empty() {
            return None;
        }
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
                        aes::cipher::generic_array::GenericArray::from_mut_slice(&mut iv_bytes);
                    cipher_ecb.decrypt_block(&mut iv_block);

                    let encrypted_data = &encrypted_bytes[16..];
                    if !encrypted_data.is_empty() {
                        let mut buf = encrypted_data.to_vec();
                        let decryptor = Decryptor::<Aes256>::new(aes_key, &iv_bytes.into());
                        match decryptor.decrypt_padded_mut::<Pkcs7>(&mut buf) {
                            Ok(plaintext) => {
                                return Some(
                                    String::from_utf8_lossy(plaintext)
                                        .trim_matches(char::from(0))
                                        .to_string(),
                                );
                            }
                            Err(e) => {
                                log::warn!("Decryption failed for {}: {:?}", enc_val, e);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                log::warn!("Failed to decode Base64 for {}: {:?}", enc_val, e);
            }
        }
        None
    }
}

impl ManifestParser for ProtobufManifestParser {
    fn parse(
        &self,
        payload_bytes: &[u8],
    ) -> Result<(ContentManifestPayload, Option<ContentManifestMetadata>)> {
        let payload_bytes_vec;
        let payload_bytes =
            if let Ok(extractor) = crate::archive::ZipArchiveExtractor::new(payload_bytes) {
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
            final_payload = payload_bytes.to_vec();
        }

        let manifest = ContentManifestPayload::parse_from_bytes(&final_payload)
            .map_err(|e| SteamVentError::Manifest(e.to_string()))?;

        Ok((manifest, metadata))
    }

    fn decrypt_filenames(
        &self,
        manifest: &mut ContentManifestPayload,
        depot_key: &[u8],
    ) -> Result<()> {
        if depot_key.len() != 32 {
            return Err(SteamVentError::Decryption(format!(
                "Invalid depot key length: {}",
                depot_key.len()
            )));
        }

        let aes_key = aes::cipher::generic_array::GenericArray::from_slice(depot_key);
        let cipher_ecb = Aes256::new(&aes_key);

        for mapping in &mut manifest.mappings {
            // Decrypt filename
            if let Some(enc_val) = mapping.filename.as_ref() {
                if let Some(decrypted) = Self::decrypt_string(enc_val, &aes_key, &cipher_ecb) {
                    log::debug!("Decrypted filename: {} -> {}", enc_val, decrypted);
                    mapping.filename = Some(decrypted);
                }
            }

            // Decrypt linktarget
            if let Some(enc_val) = mapping.linktarget.as_ref() {
                if let Some(decrypted) = Self::decrypt_string(enc_val, &aes_key, &cipher_ecb) {
                    log::debug!("Decrypted linktarget: {} -> {}", enc_val, decrypted);
                    mapping.linktarget = Some(decrypted);
                }
            }
        }
        log::info!("Decryption complete.");
        Ok(())
    }
}
