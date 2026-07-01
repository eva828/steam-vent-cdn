use crate::error::{EncryptionError, Result, SteamVentError};
use aes::Aes256;
use aes::cipher::{BlockDecrypt, BlockDecryptMut, Key, KeyInit, KeyIvInit, block_padding::Pkcs7};
use base64::{Engine as _, engine::general_purpose};
use cbc::Decryptor;
use steam_vent_proto::content_manifest::ContentManifestPayload;

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
