use crate::error::{EncryptionError, Result};
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
        let cipher_ecb = Aes256::new_from_slice(depot_key).map_err(EncryptionError::KeyInit)?;
        let aes_key = Key::<Aes256>::clone_from_slice(depot_key);

        Ok(Self {
            aes_key,
            cipher_ecb,
        })
    }

    fn decrypt_string(&self, enc_val: &str) -> Result<String> {
        if enc_val.is_empty() {
            return Err(EncryptionError::Decrypt(
                "Empty encrypted value".to_string(),
            ))?;
        }
        let enc_val_clean = enc_val.replace(['\n', '\r', ' '], "");
        let decoded_res = general_purpose::STANDARD
            .decode(&enc_val_clean)
            .or_else(|_| general_purpose::URL_SAFE.decode(&enc_val_clean));

        let encrypted_bytes = decoded_res.map_err(|e| {
            EncryptionError::Decrypt(format!("Base64 decode failed for {}: {}", enc_val, e))
        })?;

        if encrypted_bytes.len() < 16 {
            return Err(EncryptionError::Decrypt(format!(
                "Encrypted value too short for {}: {} bytes",
                enc_val,
                encrypted_bytes.len()
            )))?;
        }

        let mut iv_bytes = [0u8; 16];
        iv_bytes.copy_from_slice(&encrypted_bytes[..16]);
        let iv_block = aes::cipher::generic_array::GenericArray::from_mut_slice(&mut iv_bytes);
        self.cipher_ecb.decrypt_block(iv_block);

        let encrypted_data = &encrypted_bytes[16..];
        if encrypted_data.is_empty() {
            return Err(EncryptionError::Decrypt(format!(
                "Encrypted data empty for {}",
                enc_val
            )))?;
        }

        let mut buf = encrypted_data.to_vec();
        let decryptor = Decryptor::<Aes256>::new(&self.aes_key, (&iv_bytes).into());
        let plaintext = decryptor
            .decrypt_padded_mut::<Pkcs7>(&mut buf)
            .map_err(|e| {
                EncryptionError::Decrypt(format!("CBC decrypt failed for {}: {}", enc_val, e))
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

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::{BlockEncrypt, BlockEncryptMut, KeyInit};
    use cbc::cipher::block_padding::Pkcs7;

    type Aes256CbcEnc = cbc::Encryptor<Aes256>;

    fn make_test_key() -> [u8; 32] {
        [0xABu8; 32]
    }

    fn encrypt_filename(plain: &str, key: &[u8]) -> String {
        let iv: [u8; 16] = [0xCDu8; 16];

        let ecb = Aes256::new_from_slice(key).unwrap();
        let mut encrypted_iv = iv;
        let block = aes::cipher::generic_array::GenericArray::from_mut_slice(&mut encrypted_iv);
        ecb.encrypt_block(block);

        let cbc = Aes256CbcEnc::new_from_slices(key, &iv).unwrap();
        let data = plain.as_bytes();
        let mut buf = data.to_vec();
        let len = buf.len();
        buf.resize(len + 16, 0);
        let encrypted = cbc.encrypt_padded_mut::<Pkcs7>(&mut buf, len).unwrap();

        let mut result = encrypted_iv.to_vec();
        result.extend_from_slice(encrypted);
        base64::engine::general_purpose::STANDARD.encode(&result)
    }

    #[test]
    fn decrypt_valid_string() {
        let key = make_test_key();
        let cipher = ManifestCipher::new(&key).unwrap();
        let enc = encrypt_filename("my_workshop_file.txt", &key);
        let dec = cipher.decrypt_string(&enc).unwrap();
        assert_eq!(dec, "my_workshop_file.txt");
    }

    #[test]
    fn decrypt_fails_on_empty_input() {
        let cipher = ManifestCipher::new(&make_test_key()).unwrap();
        assert!(cipher.decrypt_string("").is_err());
    }

    #[test]
    fn decrypt_fails_on_invalid_base64() {
        let cipher = ManifestCipher::new(&make_test_key()).unwrap();
        assert!(cipher.decrypt_string("!!!invalid!!!").is_err());
    }

    #[test]
    fn decrypt_fails_on_too_short() {
        let cipher = ManifestCipher::new(&make_test_key()).unwrap();
        let short = base64::engine::general_purpose::STANDARD.encode([0x42u8; 4]);
        assert!(cipher.decrypt_string(&short).is_err());
    }

    #[test]
    fn decrypt_fails_on_wrong_key() {
        let key = make_test_key();
        let wrong_key = [0xFFu8; 32];
        let enc = encrypt_filename("test_file.bin", &key);
        let cipher = ManifestCipher::new(&wrong_key).unwrap();
        assert!(cipher.decrypt_string(&enc).is_err());
    }

    #[test]
    fn decrypt_filename_with_url_safe_base64() {
        let key = make_test_key();
        let cipher = ManifestCipher::new(&key).unwrap();
        let enc = encrypt_filename("mod/ textures.vtf", &key);

        // Encode as actual URL-safe base64 (no padding)
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&enc)
            .unwrap();
        let url_safe = base64::engine::general_purpose::URL_SAFE.encode(&raw);

        let dec = cipher.decrypt_string(&url_safe).unwrap();
        assert_eq!(dec, "mod/ textures.vtf");
    }
}
