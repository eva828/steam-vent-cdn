use crate::error::{CompressionError, EncryptionError, Result, SteamVentError};
use aes::Aes256;
use aes::cipher::{BlockDecrypt, KeyInit};
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use std::io::{Cursor, Read};

type Aes256CbcDec = cbc::Decryptor<Aes256>;

pub(crate) trait ArchiveExtractor {
    fn extract(&self) -> Result<Vec<u8>>;
}

pub(crate) struct ZipArchiveExtractor<'a> {
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
        let mut zip =
            zip::ZipArchive::new(Cursor::new(self.data)).map_err(CompressionError::Zip)?;
        let mut file = zip.by_index(0).map_err(CompressionError::Zip)?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer).map_err(SteamVentError::Io)?;
        tracing::debug!(
            "ZIP extracted successfully: {} bytes input -> {} bytes output",
            self.data.len(),
            buffer.len()
        );
        Ok(buffer)
    }
}

pub(crate) struct ZstdArchiveExtractor<'a> {
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
        let zstd_payload = &self.data[8..];
        let mut decompressed = Vec::new();
        let mut decoder = zstd::stream::Decoder::new(Cursor::new(zstd_payload))
            .map_err(CompressionError::Zstd)?
            .single_frame();
        std::io::copy(&mut decoder, &mut decompressed)?;
        tracing::debug!(
            "Zstd extracted successfully: {} bytes input -> {} bytes output",
            self.data.len(),
            decompressed.len()
        );
        Ok(decompressed)
    }
}

pub(crate) struct VZaArchiveExtractor<'a> {
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
        if !self.data.starts_with(Self::MAGIC) {
            return Err(SteamVentError::Compression(
                CompressionError::InvalidLzmaHeader("Not VZa format"),
            ));
        }

        // Reserved bytes at offset 3-6 should be zero; we skip validation.

        // Verify footer ends with 'zv' (last 2 bytes)
        let footer_zv = &self.data[self.data.len() - 2..];
        if footer_zv != b"zv" {
            return Err(SteamVentError::Compression(
                CompressionError::InvalidLzmaHeader("VZa footer does not end with 'zv'"),
            ));
        }

        // Uncompressed size from footer: bytes [len-6..len-2] (little endian)
        let uncompressed_size_bytes = &self.data[self.data.len() - 6..self.data.len() - 2];
        let uncompressed_size =
            u32::from_le_bytes(uncompressed_size_bytes.try_into().map_err(|_| {
                SteamVentError::Compression(CompressionError::InvalidLzmaHeader(
                    "Failed to parse uncompressed size from VZa footer",
                ))
            })?) as usize;

        // Compressed data: from offset 12 to len - 10
        let compressed_end = self.data.len() - 10;

        let mut decompressed = Vec::with_capacity(uncompressed_size);
        let mut options: lzma_rs::decompress::Options = Default::default();
        let unpacked_size =
            lzma_rs::decompress::UnpackedSize::UseProvided(Some(uncompressed_size as u64));
        options.unpacked_size = unpacked_size;

        // In VZa format, the properties and dictionary size (5 bytes) are at offset 7,
        // followed immediately by the compressed stream at offset 12.
        // We can pass this contiguous slice directly to the decompressor with the
        // uncompressed size set in options, entirely avoiding `std::io::Chain`.
        let mut reader = Cursor::new(&self.data[7..compressed_end]);

        if let Err(e) =
            lzma_rs::lzma_decompress_with_options(&mut reader, &mut decompressed, &options)
        {
            return Err(SteamVentError::Compression(CompressionError::Lzma(e)));
        }

        tracing::debug!(
            "VZa extracted successfully: {} bytes input -> {} bytes output",
            self.data.len(),
            decompressed.len()
        );
        Ok(decompressed)
    }
}

pub(crate) struct LzmaArchiveExtractor<'a> {
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
        lzma_rs::lzma_decompress(&mut reader, &mut decompressed).map_err(CompressionError::Lzma)?;
        tracing::debug!(
            "LZMA extracted successfully: {} bytes input -> {} bytes output",
            input_size,
            decompressed.len()
        );
        Ok(decompressed)
    }
}

pub(crate) struct RawArchiveExtractor<'a> {
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

/// Steamのチャンクデータの状態を表すenum
/// Encrypted → Decrypted → Decompressed と状態遷移していく
#[derive(Debug)]
pub enum SteamChunk {
    Encrypted(Vec<u8>),
    Decrypted(Vec<u8>),
    Decompressed(Vec<u8>),
}

impl SteamChunk {
    /// 新規作成
    pub fn new(data: Vec<u8>) -> Self {
        Self::Encrypted(data)
    }

    /// データを所有権として取得
    pub fn into_vec(self) -> Vec<u8> {
        match self {
            Self::Encrypted(v) | Self::Decrypted(v) | Self::Decompressed(v) => v,
        }
    }

    /// Encrypted → Decrypted
    /// 想定外の状態の場合はエラー
    pub fn decrypt(self, key: Option<&[u8]>) -> Result<Self> {
        match self {
            Self::Encrypted(data) => {
                if let Some(k) = key {
                    let decrypted = Self::perform_decrypt(&data, k)?;
                    Ok(Self::Decrypted(decrypted))
                } else {
                    // キーがない場合はそのままDecryptedに移行
                    Ok(Self::Decrypted(data))
                }
            }
            Self::Decrypted(_) => Err(SteamVentError::Chunk(
                crate::error::ChunkError::UnexpectedState {
                    expected: "Encrypted",
                    actual: "Decrypted",
                },
            )),
            Self::Decompressed(_) => Err(SteamVentError::Chunk(
                crate::error::ChunkError::UnexpectedState {
                    expected: "Encrypted",
                    actual: "Decompressed",
                },
            )),
        }
    }

    /// Decrypted → Decompressed（整合性検証付き）
    pub fn decompress_and_verify(
        self,
        expected_uncompressed_size: u32,
        expected_crc: u32,
    ) -> Result<Self> {
        match self {
            Self::Decrypted(data) => {
                let extractor = Self::get_extractor(&data)?;
                let decompressed = extractor.extract()?;

                // Verify uncompressed size matches manifest's cb_original
                let actual_size = decompressed.len() as u32;
                if actual_size != expected_uncompressed_size {
                    return Err(SteamVentError::Chunk(
                        crate::error::ChunkError::SizeMismatch {
                            expected: expected_uncompressed_size,
                            actual: actual_size,
                        },
                    ));
                }

                // Verify Adler32 checksum matches manifest's crc
                let mut hasher = adler::Adler32::from_checksum(0);
                hasher.write_slice(&decompressed);
                let actual_crc = hasher.checksum();
                if actual_crc != expected_crc {
                    return Err(SteamVentError::Chunk(
                        crate::error::ChunkError::ChecksumMismatch {
                            expected: expected_crc,
                            actual: actual_crc,
                        },
                    ));
                }

                Ok(Self::Decompressed(decompressed))
            }
            Self::Encrypted(_) => Err(SteamVentError::Chunk(
                crate::error::ChunkError::UnexpectedState {
                    expected: "Decrypted",
                    actual: "Encrypted",
                },
            )),
            Self::Decompressed(_) => Err(SteamVentError::Chunk(
                crate::error::ChunkError::UnexpectedState {
                    expected: "Decrypted",
                    actual: "Decompressed",
                },
            )),
        }
    }

    /// 実際の復号処理
    fn perform_decrypt(data: &[u8], key: &[u8]) -> Result<Vec<u8>> {
        if data.len() < 16 {
            return Ok(data.to_vec());
        }

        let (iv_encrypted, ciphertext) = data.split_at(16);

        let ecb_cipher = Aes256::new_from_slice(key)
            .map_err(EncryptionError::KeyInit)
            .map_err(SteamVentError::Encryption)?;

        let mut iv = iv_encrypted.to_vec();
        let block = aes::cipher::generic_array::GenericArray::from_mut_slice(&mut iv);
        ecb_cipher.decrypt_block(block);

        let cipher = Aes256CbcDec::new_from_slices(key, &iv)
            .map_err(EncryptionError::IvInit)
            .map_err(SteamVentError::Encryption)?;

        let mut buffer = ciphertext.to_vec();
        match cipher.decrypt_padded_mut::<cbc::cipher::block_padding::Pkcs7>(&mut buffer) {
            Ok(decrypted) => Ok(decrypted.to_vec()),
            Err(_) => {
                let magic_bytes = &data[..data.len().min(4)];
                Err(SteamVentError::Encryption(EncryptionError::Decrypt(
                    format!(
                        "Chunk decryption failed: unpadding error (input size: {} bytes, magic bytes: {:02X?})",
                        data.len(),
                        magic_bytes
                    ),
                )))
            }
        }
    }

    fn get_extractor(data: &[u8]) -> Result<Box<dyn ArchiveExtractor + '_>> {
        match data {
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
}
