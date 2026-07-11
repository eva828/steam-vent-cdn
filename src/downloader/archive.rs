use crate::error::{ChunkError, CompressionError, EncryptionError, Result, SteamVentError};
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

    pub(crate) fn new(data: &'a [u8]) -> Result<Self> {
        if !data.starts_with(Self::MAGIC) {
            return Err(CompressionError::InvalidMagic {
                expected: Self::MAGIC,
            })?;
        }
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

    pub(crate) fn new(data: &'a [u8]) -> Result<Self> {
        let (magic, _rest) = data
            .split_first_chunk::<4>()
            .ok_or(CompressionError::InvalidData("Zstd data too short"))?;
        if magic != Self::MAGIC {
            return Err(CompressionError::InvalidMagic {
                expected: Self::MAGIC,
            })?;
        }
        Ok(Self { data })
    }
}

impl<'a> ArchiveExtractor for ZstdArchiveExtractor<'a> {
    fn extract(&self) -> Result<Vec<u8>> {
        let (_header, zstd_payload) =
            self.data
                .split_first_chunk::<8>()
                .ok_or(CompressionError::InvalidData(
                    "Zstd data too short for header",
                ))?;
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

    pub(crate) fn new(data: &'a [u8]) -> Result<Self> {
        let (magic, _rest) = data
            .split_first_chunk::<3>()
            .ok_or(CompressionError::InvalidData("VZa data too short"))?;
        if magic != Self::MAGIC {
            return Err(CompressionError::InvalidMagic {
                expected: Self::MAGIC,
            })?;
        }
        Ok(Self { data })
    }
}

impl<'a> ArchiveExtractor for VZaArchiveExtractor<'a> {
    fn extract(&self) -> Result<Vec<u8>> {
        let (_header, rest) =
            self.data
                .split_first_chunk::<7>()
                .ok_or(CompressionError::InvalidData(
                    "VZa data too short for header",
                ))?;
        let (compressed, footer) =
            rest.split_last_chunk::<10>()
                .ok_or(CompressionError::InvalidData(
                    "VZa data too short for footer",
                ))?;
        let (rest_of_footer, zv_magic) =
            footer
                .split_last_chunk::<2>()
                .ok_or(CompressionError::InvalidData(
                    "VZa data too short for footer magic",
                ))?;
        if zv_magic != b"zv" {
            return Err(CompressionError::InvalidData(
                "VZa footer does not end with 'zv'",
            ))?;
        }
        let (_reserved, uncompressed_size_bytes) =
            rest_of_footer
                .split_last_chunk::<4>()
                .ok_or(CompressionError::InvalidData(
                    "VZa data too short for uncompressed size",
                ))?;
        let uncompressed_size = u32::from_le_bytes(*uncompressed_size_bytes) as usize;

        let mut decompressed = Vec::with_capacity(uncompressed_size);
        let mut options: lzma_rs::decompress::Options = Default::default();
        let unpacked_size =
            lzma_rs::decompress::UnpackedSize::UseProvided(Some(uncompressed_size as u64));
        options.unpacked_size = unpacked_size;

        let mut reader = Cursor::new(compressed);

        if let Err(e) =
            lzma_rs::lzma_decompress_with_options(&mut reader, &mut decompressed, &options)
        {
            return Err(CompressionError::Lzma(e))?;
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
}

impl<'a> LzmaArchiveExtractor<'a> {
    pub const MAGIC: &'static [u8] = &[0x5D, 0x00, 0x00, 0x80, 0x00];

    pub(crate) fn new(data: &'a [u8]) -> Result<Self> {
        if !data.starts_with(Self::MAGIC) {
            return Err(CompressionError::InvalidMagic {
                expected: Self::MAGIC,
            })?;
        }
        Ok(Self { data })
    }
}

impl<'a> ArchiveExtractor for LzmaArchiveExtractor<'a> {
    fn extract(&self) -> Result<Vec<u8>> {
        let input_size = self.data.len();
        let mut reader = Cursor::new(self.data);
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
    pub(crate) fn new(data: &'a [u8]) -> Result<Self> {
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

pub(crate) fn create_archive_extractor(data: &[u8]) -> Result<Box<dyn ArchiveExtractor + '_>> {
    if let Ok(ext) = ZipArchiveExtractor::new(data) {
        return Ok(Box::new(ext));
    }
    if let Ok(ext) = ZstdArchiveExtractor::new(data) {
        return Ok(Box::new(ext));
    }
    if let Ok(ext) = VZaArchiveExtractor::new(data) {
        return Ok(Box::new(ext));
    }
    if let Ok(ext) = LzmaArchiveExtractor::new(data) {
        return Ok(Box::new(ext));
    }
    Ok(Box::new(RawArchiveExtractor::new(data)?))
}

#[derive(Debug)]
pub(crate) enum SteamChunk {
    Encrypted(Vec<u8>),
    Decrypted(Vec<u8>),
    Decompressed(Vec<u8>),
}

impl SteamChunk {
    pub(crate) fn new(data: Vec<u8>) -> Self {
        Self::Encrypted(data)
    }

    pub(crate) fn into_vec(self) -> Vec<u8> {
        match self {
            Self::Encrypted(v) | Self::Decrypted(v) | Self::Decompressed(v) => v,
        }
    }

    pub(crate) fn decrypt(self, key: Option<&[u8]>) -> Result<Self> {
        match self {
            Self::Encrypted(data) => {
                if let Some(k) = key {
                    let decrypted = Self::perform_decrypt(&data, k)?;
                    Ok(Self::Decrypted(decrypted))
                } else {
                    Ok(Self::Decrypted(data))
                }
            }
            Self::Decrypted(_) => Err(ChunkError::UnexpectedState {
                expected: "Encrypted",
                actual: "Decrypted",
            })?,
            Self::Decompressed(_) => Err(ChunkError::UnexpectedState {
                expected: "Encrypted",
                actual: "Decompressed",
            })?,
        }
    }

    pub(crate) fn decompress_and_verify(
        self,
        expected_uncompressed_size: u32,
        expected_crc: u32,
    ) -> Result<Self> {
        match self {
            Self::Decrypted(data) => {
                let extractor = create_archive_extractor(&data)?;
                let decompressed = extractor.extract()?;

                let actual_size = decompressed.len() as u32;
                if actual_size != expected_uncompressed_size {
                    return Err(ChunkError::SizeMismatch {
                        expected: expected_uncompressed_size,
                        actual: actual_size,
                    })?;
                }

                let mut hasher = adler::Adler32::from_checksum(0);
                hasher.write_slice(&decompressed);
                let actual_crc = hasher.checksum();
                if actual_crc != expected_crc {
                    return Err(ChunkError::ChecksumMismatch {
                        expected: expected_crc,
                        actual: actual_crc,
                    })?;
                }

                Ok(Self::Decompressed(decompressed))
            }
            Self::Encrypted(_) => Err(ChunkError::UnexpectedState {
                expected: "Decrypted",
                actual: "Encrypted",
            })?,
            Self::Decompressed(_) => Err(ChunkError::UnexpectedState {
                expected: "Decrypted",
                actual: "Decompressed",
            })?,
        }
    }

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
                Err(EncryptionError::Decrypt(format!(
                    "Chunk decryption failed: unpadding error (input size: {} bytes, magic bytes: {:02X?})",
                    data.len(),
                    magic_bytes
                )))?
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{ChunkError, SteamVentError};
    use aes::cipher::{BlockEncrypt, BlockEncryptMut, KeyInit};
    use cbc::cipher::block_padding::Pkcs7;
    use std::io::Write;

    type Aes256CbcEnc = cbc::Encryptor<Aes256>;

    fn make_test_key() -> [u8; 32] {
        [0x42u8; 32]
    }

    fn make_encrypted_test_data(plaintext: &[u8], key: &[u8]) -> Vec<u8> {
        let iv: [u8; 16] = [0xABu8; 16];

        let ecb_cipher = Aes256::new_from_slice(key).unwrap();
        let mut encrypted_iv = iv;
        let block = aes::cipher::generic_array::GenericArray::from_mut_slice(&mut encrypted_iv);
        ecb_cipher.encrypt_block(block);

        let cbc_cipher = Aes256CbcEnc::new_from_slices(key, &iv).unwrap();
        let mut buf = plaintext.to_vec();
        let len = buf.len();
        buf.resize(len + 16, 0);
        let encrypted = cbc_cipher
            .encrypt_padded_mut::<Pkcs7>(&mut buf, len)
            .unwrap();

        let mut result = encrypted_iv.to_vec();
        result.extend_from_slice(encrypted);
        result
    }

    fn make_zip(data: &[u8]) -> Vec<u8> {
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("f", opts).unwrap();
        zip.write_all(data).unwrap();
        let finished = zip.finish().unwrap();
        finished.into_inner()
    }

    fn adler32(data: &[u8]) -> u32 {
        let mut h = adler::Adler32::from_checksum(0);
        h.write_slice(data);
        h.checksum()
    }

    #[test]
    fn decrypt_with_key_roundtrips() {
        let original = b"hello world, this is test data for steam chunk decryption!";
        let key = make_test_key();
        let encrypted = make_encrypted_test_data(original, &key);

        let chunk = SteamChunk::new(encrypted);
        let result = chunk.decrypt(Some(&key)).unwrap();
        assert!(matches!(result, SteamChunk::Decrypted(ref d) if d == original));
    }

    #[test]
    fn decrypt_without_key_passes_through() {
        let data = b"raw unencrypted chunk".to_vec();
        let chunk = SteamChunk::new(data.clone());
        let result = chunk.decrypt(None).unwrap();
        assert!(matches!(result, SteamChunk::Decrypted(ref d) if d == &data));
    }

    #[test]
    fn decrypt_short_data_passes_through() {
        let data = b"shrt".to_vec();
        let key = make_test_key();
        let chunk = SteamChunk::new(data.clone());
        let result = chunk.decrypt(Some(&key)).unwrap();
        assert!(matches!(result, SteamChunk::Decrypted(ref d) if d == &data));
    }

    #[test]
    fn decrypt_wrong_key_fails() {
        let original = b"data encrypted with one key, decrypted with another";
        let key = make_test_key();
        let wrong_key = [0xFFu8; 32];
        let encrypted = make_encrypted_test_data(original, &key);

        let chunk = SteamChunk::new(encrypted);
        assert!(chunk.decrypt(Some(&wrong_key)).is_err());
    }

    #[test]
    fn decrypt_on_decrypted_state_errors() {
        let chunk = SteamChunk::new(b"data".to_vec());
        let chunk = chunk.decrypt(None).unwrap();
        assert!(matches!(
            chunk.decrypt(None),
            Err(SteamVentError::Chunk(ChunkError::UnexpectedState { .. }))
        ));
    }

    #[test]
    fn decompress_and_verify_zip_roundtrips() {
        let original = b"zip compressed steam chunk test payload";
        let compressed = make_zip(original);
        let crc = adler32(original);

        let chunk = SteamChunk::new(compressed);
        let chunk = chunk.decrypt(None).unwrap();
        let result = chunk
            .decompress_and_verify(original.len() as u32, crc)
            .unwrap();
        assert!(matches!(result, SteamChunk::Decompressed(ref d) if d == original));
    }

    #[test]
    fn decompress_and_verify_checksum_mismatch_errors() {
        let original = b"checksum will be wrong";
        let compressed = make_zip(original);

        let chunk = SteamChunk::new(compressed);
        let chunk = chunk.decrypt(None).unwrap();
        assert!(matches!(
            chunk.decompress_and_verify(original.len() as u32, 0xDEADBEEF),
            Err(SteamVentError::Chunk(ChunkError::ChecksumMismatch { .. }))
        ));
    }

    #[test]
    fn decompress_and_verify_size_mismatch_errors() {
        let original = b"size will be reported wrong";
        let compressed = make_zip(original);
        let crc = adler32(original);

        let chunk = SteamChunk::new(compressed);
        let chunk = chunk.decrypt(None).unwrap();
        assert!(matches!(
            chunk.decompress_and_verify((original.len() + 100) as u32, crc),
            Err(SteamVentError::Chunk(ChunkError::SizeMismatch { .. }))
        ));
    }

    #[test]
    fn decompress_and_verify_on_encrypted_errors() {
        let chunk = SteamChunk::new(b"encrypted not decrypted".to_vec());
        assert!(matches!(
            chunk.decompress_and_verify(0, 0),
            Err(SteamVentError::Chunk(ChunkError::UnexpectedState { .. }))
        ));
    }

    #[test]
    fn full_encrypt_decrypt_decompress_pipeline() {
        let original = b"full pipeline: encrypt, decrypt, decompress, verify";
        let key = make_test_key();
        let compressed = make_zip(original);
        let encrypted = make_encrypted_test_data(&compressed, &key);
        let crc = adler32(original);

        let chunk = SteamChunk::new(encrypted);
        let chunk = chunk.decrypt(Some(&key)).unwrap();
        let result = chunk
            .decompress_and_verify(original.len() as u32, crc)
            .unwrap();
        assert!(matches!(result, SteamChunk::Decompressed(ref d) if d == original));
    }

    #[test]
    fn archive_detector_prefers_zip_over_raw() {
        let data = make_zip(b"zip or raw");
        let extractor = create_archive_extractor(&data).unwrap();
        assert_eq!(extractor.extract().unwrap(), b"zip or raw");
    }

    #[test]
    fn archive_detector_handles_vsza() {
        let original = b"zstd vsza container test";
        let zstd_data = zstd::stream::encode_all(original.as_ref(), 3).unwrap();
        let mut vsza = b"VSZa\0\0\0\0".to_vec();
        vsza.extend_from_slice(&zstd_data);

        let extractor = create_archive_extractor(&vsza).unwrap();
        assert_eq!(extractor.extract().unwrap(), original);
    }

    #[test]
    fn archive_detector_handles_vza() {
        let original = b"vza format test payload for archive extractor detection";
        let mut lzma_data = Vec::new();
        lzma_rs::lzma_compress_with_options(
            &mut std::io::BufReader::new(std::io::Cursor::new(original)),
            &mut lzma_data,
            &lzma_rs::compress::Options {
                unpacked_size: lzma_rs::compress::UnpackedSize::SkipWritingToHeader,
            },
        )
        .unwrap();

        let mut vza = vec![b'V', b'Z', b'a', 0, 0, 0, 0];
        vza.extend_from_slice(&lzma_data);
        vza.extend_from_slice(&[0u8; 4]);
        vza.extend_from_slice(&(original.len() as u32).to_le_bytes());
        vza.extend_from_slice(b"zv");

        let extractor = create_archive_extractor(&vza).unwrap();
        let result = extractor.extract().unwrap();
        assert_eq!(result, original);
    }

    #[test]
    fn archive_detector_handles_lzma() {
        let original = b"plain lzma data";
        let mut compressed = Vec::new();
        lzma_rs::lzma_compress(&mut std::io::Cursor::new(original), &mut compressed).unwrap();

        let extractor = create_archive_extractor(&compressed).unwrap();
        assert_eq!(extractor.extract().unwrap(), original);
    }

    #[test]
    fn archive_detector_falls_back_to_raw() {
        let data = b"not a recognized archive format at all";
        let extractor = create_archive_extractor(data).unwrap();
        assert_eq!(extractor.extract().unwrap(), data);
    }
}
