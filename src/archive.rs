use crate::error::{Result, SteamVentError};
use aes::cipher::{BlockDecrypt, KeyInit};
use aes::Aes256;
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
    pub const MAGIC: &'static [u8] = b"PK\x03\x04";

    pub fn new(data: &'a [u8]) -> Result<Self> {
        Ok(Self { data })
    }
}

impl<'a> ArchiveExtractor for ZipArchiveExtractor<'a> {
    fn extract(&self) -> Result<Vec<u8>> {
        let mut zip = zip::ZipArchive::new(Cursor::new(self.data))
            .map_err(|e| SteamVentError::Manifest(e.to_string()))?;
        let mut file = zip
            .by_index(0)
            .map_err(|e| SteamVentError::Manifest(e.to_string()))?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)
            .map_err(|e| SteamVentError::Io(e))?;
        tracing::debug!(
            "ZIP extracted successfully: {} bytes input -> {} bytes output",
            self.data.len(),
            buffer.len()
        );
        Ok(buffer)
    }
}

pub struct ZstdArchiveExtractor<'a> {
    data: &'a [u8],
}

impl<'a> ZstdArchiveExtractor<'a> {
    pub const MAGIC: &'static [u8] = b"VSZa";

    pub fn new(data: &'a [u8]) -> Result<Self> {
        Ok(Self { data })
    }
}

impl<'a> ArchiveExtractor for ZstdArchiveExtractor<'a> {
    fn extract(&self) -> Result<Vec<u8>> {
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
        tracing::debug!(
            "Zstd extracted successfully: {} bytes input -> {} bytes output",
            self.data.len(),
            decompressed.len()
        );
        Ok(decompressed)
    }
}

pub struct VZaArchiveExtractor<'a> {
    data: &'a [u8],
}

impl<'a> VZaArchiveExtractor<'a> {
    pub const MAGIC: &'static [u8] = b"VZa";

    pub fn new(data: &'a [u8]) -> Result<Self> {
        Ok(Self { data })
    }
}

impl<'a> ArchiveExtractor for VZaArchiveExtractor<'a> {
    fn extract(&self) -> Result<Vec<u8>> {
        // Minimum VZa structure: 7 bytes header + 5 props + 0 compressed + 10 footer = 22
        if self.data.len() < 22 {
            return Err(SteamVentError::Cdn(format!(
                "VZa data too small ({} bytes, minimum required: 22 bytes)",
                self.data.len()
            )));
        }

        if !self.data.starts_with(Self::MAGIC) {
            let magic_bytes = &self.data[..self.data.len().min(4)];
            return Err(SteamVentError::Cdn(format!(
                "Not VZa format (magic bytes: {:02X?})",
                magic_bytes
            )));
        }

        // Reserved bytes at offset 3-6 should be zero; we skip validation.

        // Compressed data: from offset 12 to len - 10
        let compressed_start = 12;
        let compressed_end = self.data.len() - 10;
        if compressed_end <= compressed_start {
            return Err(SteamVentError::Cdn(format!(
                "VZa compressed region empty (total: {} bytes, start: {}, end: {})",
                self.data.len(),
                compressed_start,
                compressed_end
            )));
        }

        // Verify footer ends with 'zv' (last 2 bytes)
        let footer_zv = &self.data[self.data.len() - 2..];
        if footer_zv != b"zv" {
            return Err(SteamVentError::Cdn(format!(
                "VZa footer does not end with 'zv': {:02X?}",
                footer_zv
            )));
        }

        // Uncompressed size from footer: bytes [len-6..len-2] (little endian)
        let uncompressed_size_bytes = &self.data[self.data.len() - 6..self.data.len() - 2];
        let uncompressed_size =
            u32::from_le_bytes(uncompressed_size_bytes.try_into().unwrap_or([0; 4])) as usize;

        // If size is zero or unreasonably large, return error
        if uncompressed_size == 0 || uncompressed_size > 100_000_000 {
            return Err(SteamVentError::Cdn(format!(
                "Suspicious uncompressed size {} (max 100MB)",
                uncompressed_size
            )));
        }

        let compressed_size = compressed_end - compressed_start;

        let mut decompressed = Vec::with_capacity(uncompressed_size);
        let mut options: lzma_rs::decompress::Options = Default::default();
        options.unpacked_size =
            lzma_rs::decompress::UnpackedSize::UseProvided(Some(uncompressed_size as u64));

        // In VZa format, the properties and dictionary size (5 bytes) are at offset 7,
        // followed immediately by the compressed stream at offset 12.
        // We can pass this contiguous slice directly to the decompressor with the
        // uncompressed size set in options, entirely avoiding `std::io::Chain`.
        let mut reader = Cursor::new(&self.data[7..compressed_end]);

        if let Err(e) =
            lzma_rs::lzma_decompress_with_options(&mut reader, &mut decompressed, &options)
        {
            return Err(SteamVentError::Cdn(format!(
                "VZa lzma_decompress failed: {}",
                e
            )));
        }

        if decompressed.len() == uncompressed_size {
            tracing::debug!(
                "VZa extracted successfully: {} bytes input -> {} bytes output",
                compressed_size,
                decompressed.len()
            );
            Ok(decompressed)
        } else {
            return Err(SteamVentError::Cdn(format!(
                "VZa decode incomplete: written={}, expected={}",
                decompressed.len(),
                uncompressed_size
            )));
        }
    }
}

pub struct LzmaArchiveExtractor<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> LzmaArchiveExtractor<'a> {
    pub const MAGIC: &'static [u8] = &[0x5D, 0x00, 0x00, 0x80, 0x00];

    pub fn new(data: &'a [u8], offset: usize) -> Result<Self> {
        Ok(Self { data, offset })
    }
}

impl<'a> ArchiveExtractor for LzmaArchiveExtractor<'a> {
    fn extract(&self) -> Result<Vec<u8>> {
        let input_size = self.data.len() - self.offset;
        let mut reader = Cursor::new(&self.data[self.offset..]);
        let mut decompressed = Vec::new();
        lzma_rs::lzma_decompress(&mut reader, &mut decompressed)
            .map_err(|e| SteamVentError::Cdn(format!("LZMA Decompression failed: {}", e)))?;
        tracing::debug!(
            "LZMA extracted successfully: {} bytes input -> {} bytes output",
            input_size,
            decompressed.len()
        );
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
        let output = self.data.to_vec();
        tracing::debug!(
            "Raw extracted successfully: {} bytes input -> {} bytes output",
            self.data.len(),
            output.len()
        );
        Ok(output)
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
        match self.data {
            d if d.starts_with(ZipArchiveExtractor::MAGIC) => {
                Ok(Box::new(ZipArchiveExtractor::new(d)?))
            }
            d if d.starts_with(ZstdArchiveExtractor::MAGIC) => {
                Ok(Box::new(ZstdArchiveExtractor::new(d)?))
            }
            d if d.starts_with(VZaArchiveExtractor::MAGIC) => {
                Ok(Box::new(VZaArchiveExtractor::new(d)?))
            }
            d if d.starts_with(LzmaArchiveExtractor::MAGIC) => {
                Ok(Box::new(LzmaArchiveExtractor::new(d, 0)?))
            }
            d => Ok(Box::new(RawArchiveExtractor::new(d)?)),
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
                    let magic_bytes = &self.data[..self.data.len().min(4)];
                    tracing::error!(
                        "Chunk decryption failed: {} (input size: {} bytes, magic bytes: {:02X?})",
                        e,
                        self.data.len(),
                        magic_bytes
                    );
                    return Err(SteamVentError::Decryption(format!(
                        "Chunk decryption failed: {} (input size: {} bytes, magic bytes: {:02X?})",
                        e,
                        self.data.len(),
                        magic_bytes
                    )));
                }
            }
        }

        let extractor = extractor_chunk.get_extractor()?;
        extractor.extract()
    }
}
