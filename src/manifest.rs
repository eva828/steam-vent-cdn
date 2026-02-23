use crate::archive::ArchiveExtractor;
use crate::error::{Result, SteamVentError};
use aes::cipher::{block_padding::Pkcs7, BlockDecrypt, BlockDecryptMut, KeyInit, KeyIvInit};
use aes::Aes256;
use base64::{engine::general_purpose, Engine as _};
use cbc::Decryptor;
use steam_vent_proto::content_manifest::{ContentManifestMetadata, ContentManifestPayload};
use steam_vent_proto::protobuf::Message;

const MAGIC_PAYLOAD: u32 = 0x71F617D0;
const MAGIC_METADATA_1: u32 = 0x1F448235;
const MAGIC_METADATA_2: u32 = 0x1F4812BE;
const MAGIC_SIGNATURE_1: u32 = 0x1A670D5A;
const MAGIC_SIGNATURE_2: u32 = 0x1B81B817;

fn decrypt_string(
    enc_val: &str,
    aes_key: &aes::cipher::generic_array::GenericArray<u8, aes::cipher::typenum::U32>,
    cipher_ecb: &Aes256,
) -> Result<String> {
    if enc_val.is_empty() {
        return Err(SteamVentError::Decryption(
            "Empty encrypted value".to_string(),
        ));
    }
    let enc_val_clean = enc_val.replace(['\n', '\r', ' '], "");
    let decoded_res = general_purpose::STANDARD
        .decode(&enc_val_clean)
        .or_else(|_| general_purpose::URL_SAFE.decode(&enc_val_clean));

    let encrypted_bytes = decoded_res.map_err(|e| {
        SteamVentError::Decryption(format!("Failed to decode Base64 for {}: {}", enc_val, e))
    })?;

    if encrypted_bytes.len() < 16 {
        return Err(SteamVentError::Decryption(format!(
            "Encrypted value too short for {}: {} bytes",
            enc_val,
            encrypted_bytes.len()
        )));
    }

    let mut iv_bytes = [0u8; 16];
    iv_bytes.copy_from_slice(&encrypted_bytes[..16]);
    let mut iv_block = aes::cipher::generic_array::GenericArray::from_mut_slice(&mut iv_bytes);
    #[allow(clippy::needless_borrow)]
    cipher_ecb.decrypt_block(&mut iv_block);

    let encrypted_data = &encrypted_bytes[16..];
    if encrypted_data.is_empty() {
        return Err(SteamVentError::Decryption(format!(
            "Encrypted data empty for {}",
            enc_val
        )));
    }

    let mut buf = encrypted_data.to_vec();
    let decryptor = Decryptor::<Aes256>::new(aes_key, (&iv_bytes).into());
    let plaintext = decryptor
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| {
            SteamVentError::Decryption(format!("Decryption failed for {}: {}", enc_val, e))
        })?;

    let clean_bytes = match plaintext.iter().position(|&b| b == 0) {
        Some(pos) => &plaintext[..pos],
        None => plaintext,
    };

    Ok(String::from_utf8_lossy(clean_bytes).into_owned())
}

pub(crate) fn parse_manifest(
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
            return Err(SteamVentError::Manifest(format!(
                "Segment length {} exceeds remaining data {}",
                length,
                payload_bytes.len() - cursor - 8
            )));
        }

        let segment_data = &payload_bytes[cursor + 8..cursor + 8 + length];

        if magic == MAGIC_PAYLOAD {
            tracing::debug!("Processing Payload segment");
            final_payload.extend_from_slice(segment_data);
        } else if magic == MAGIC_METADATA_1 || magic == MAGIC_METADATA_2 {
            tracing::debug!("Decoding Metadata segment");
            let m = ContentManifestMetadata::parse_from_bytes(segment_data).map_err(|e| {
                SteamVentError::Manifest(format!("Failed to decode Metadata segment: {}", e))
            })?;
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
            return Err(SteamVentError::Manifest(format!(
                "Unknown segment magic: 0x{:08X}",
                magic
            )));
        }

        cursor += 8 + length;
    }

    if final_payload.is_empty() {
        tracing::error!("No Payload segment found, manifest is invalid");
        return Err(SteamVentError::Manifest(
            "No Payload segment found in manifest".to_string(),
        ));
    }

    let manifest = ContentManifestPayload::parse_from_bytes(&final_payload)
        .map_err(|e| SteamVentError::Manifest(e.to_string()))?;

    Ok((manifest, metadata))
}

pub(crate) fn decrypt_manifest_filenames(
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
    let cipher_ecb = Aes256::new(aes_key);

    for mapping in &mut manifest.mappings {
        if let Some(enc_val) = mapping.filename.as_ref() {
            #[allow(clippy::needless_borrow)]
            let decrypted = decrypt_string(enc_val, &aes_key, &cipher_ecb)?;
            tracing::debug!("Decrypted filename: {} -> {}", enc_val, decrypted);
            mapping.filename = Some(decrypted);
        }

        if let Some(enc_val) = mapping.linktarget.as_ref() {
            #[allow(clippy::needless_borrow)]
            let decrypted = decrypt_string(enc_val, &aes_key, &cipher_ecb)?;
            tracing::debug!("Decrypted linktarget: {} -> {}", enc_val, decrypted);
            mapping.linktarget = Some(decrypted);
        }
    }
    tracing::debug!("Decryption complete.");
    Ok(())
}
