use crate::error::{Result, SteamVentError};
use aes::Aes256;
use aes::cipher::{BlockDecrypt, KeyInit};
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use std::io::{Cursor, Read};

type Aes256CbcDec = cbc::Decryptor<Aes256>;

pub trait ArchiveExtractor {
    fn extract(&self) -> Result<Vec<u8>>;
}

pub struct ZipArchiveExtractor<'a> {
    data: &'a [u8],
}

impl<'a> ZipArchiveExtractor<'a> {
    pub fn new(data: &'a [u8]) -> Result<Self> {
        Ok(Self { data })
    }
}

impl<'a> ArchiveExtractor for ZipArchiveExtractor<'a> {
    fn extract(&self) -> Result<Vec<u8>> {
        log::debug!("Unzipping data...");
        let mut zip = zip::ZipArchive::new(Cursor::new(self.data))
            .map_err(|e| SteamVentError::Manifest(e.to_string()))?;
        let mut file = zip
            .by_index(0)
            .map_err(|e| SteamVentError::Manifest(e.to_string()))?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)
            .map_err(|e| SteamVentError::Io(e))?;
        Ok(buffer)
    }
}

pub struct ZstdArchiveExtractor<'a> {
    data: &'a [u8],
}

impl<'a> ZstdArchiveExtractor<'a> {
    pub fn new(data: &'a [u8]) -> Result<Self> {
        Ok(Self { data })
    }
}

impl<'a> ArchiveExtractor for ZstdArchiveExtractor<'a> {
    fn extract(&self) -> Result<Vec<u8>> {
        log::debug!("Applying Zstd decompression...");
        if self.data.len() <= 8 {
            return Err(SteamVentError::Cdn("Zstd chunk too small?".to_string()));
        }
        let zstd_payload = &self.data[8..];
        let mut decompressed = Vec::new();
        let mut decoder = zstd::stream::Decoder::new(Cursor::new(zstd_payload))
            .map_err(|e| SteamVentError::Cdn(format!("Zstd Decompression failed (init): {}", e)))?
            .single_frame();
        std::io::copy(&mut decoder, &mut decompressed)
            .map_err(|e| SteamVentError::Cdn(format!("Zstd Decompression failed (copy): {}", e)))?;
        Ok(decompressed)
    }
}

pub struct LzmaArchiveExtractor<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> LzmaArchiveExtractor<'a> {
    pub fn new(data: &'a [u8], offset: usize) -> Result<Self> {
        Ok(Self { data, offset })
    }
}

impl<'a> ArchiveExtractor for LzmaArchiveExtractor<'a> {
    fn extract(&self) -> Result<Vec<u8>> {
        log::debug!("Applying LZMA decompression...");
        let mut reader = Cursor::new(&self.data[self.offset..]);
        let mut decompressed = Vec::new();
        lzma_rs::lzma_decompress(&mut reader, &mut decompressed)
            .map_err(|e| SteamVentError::Cdn(format!("LZMA Decompression failed: {}", e)))?;
        Ok(decompressed)
    }
}

pub struct RawArchiveExtractor<'a> {
    data: &'a [u8],
}

impl<'a> RawArchiveExtractor<'a> {
    pub fn new(data: &'a [u8]) -> Result<Self> {
        Ok(Self { data })
    }
}

impl<'a> ArchiveExtractor for RawArchiveExtractor<'a> {
    fn extract(&self) -> Result<Vec<u8>> {
        log::debug!("No compression signature found. Treating as raw data.");
        Ok(self.data.to_vec())
    }
}

pub struct SteamChunk<'a> {
    data: &'a [u8],
}

impl<'a> SteamChunk<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    pub fn decrypt(&self, key: &[u8]) -> Result<Vec<u8>> {
        if self.data.len() < 16 {
            return Ok(self.data.to_vec());
        }

        let (iv_encrypted, ciphertext) = self.data.split_at(16);

        // The first 16 bytes are the IV encrypted in ECB mode
        let ecb_cipher = Aes256::new_from_slice(key)
            .map_err(|_| SteamVentError::Cdn("Key init failed".to_string()))?;

        let mut iv = iv_encrypted.to_vec();
        let block = aes::cipher::generic_array::GenericArray::from_mut_slice(&mut iv);
        ecb_cipher.decrypt_block(block);

        let cipher = Aes256CbcDec::new_from_slices(key, &iv)
            .map_err(|_| SteamVentError::Cdn("Key or IV init failed".to_string()))?;

        let mut buffer = ciphertext.to_vec();
        match cipher.decrypt_padded_mut::<cbc::cipher::block_padding::Pkcs7>(&mut buffer) {
            Ok(decrypted) => Ok(decrypted.to_vec()),
            Err(_) => {
                // Unpadding failed
                Err(SteamVentError::Cdn(
                    "Decryption or Unpadding failed".to_string(),
                ))
            }
        }
    }

    pub fn get_extractor(&self) -> Result<Box<dyn ArchiveExtractor + 'a>> {
        if self.data.starts_with(b"PK\x03\x04") {
            Ok(Box::new(ZipArchiveExtractor::new(self.data)?))
        } else if self.data.starts_with(b"VSZa") {
            Ok(Box::new(ZstdArchiveExtractor::new(self.data)?))
        } else if self.data.starts_with(&[0x5D, 0x00, 0x00, 0x80, 0x00]) {
            Ok(Box::new(LzmaArchiveExtractor::new(self.data, 0)?))
        } else if let Some(pos) = self
            .data
            .windows(5)
            .position(|w| w == [0x5D, 0x00, 0x00, 0x80, 0x00])
        {
            Ok(Box::new(LzmaArchiveExtractor::new(self.data, pos)?))
        } else {
            Ok(Box::new(RawArchiveExtractor::new(self.data)?))
        }
    }

    pub fn decode(&self, depot_key: Option<&[u8]>) -> Result<Vec<u8>> {
        let decrypted_buffer;
        let mut extractor_chunk = self;
        let temp_chunk;

        if let Some(key) = depot_key {
            match self.decrypt(key) {
                Ok(decrypted) => {
                    decrypted_buffer = decrypted;
                    temp_chunk = SteamChunk::new(&decrypted_buffer);
                    extractor_chunk = &temp_chunk;
                }
                Err(e) => {
                    log::warn!(
                        "Chunk decryption failed, attempting to read as raw/compressed data: {}",
                        e
                    );
                }
            }
        }

        let extractor = extractor_chunk.get_extractor()?;
        extractor.extract()
    }
}
