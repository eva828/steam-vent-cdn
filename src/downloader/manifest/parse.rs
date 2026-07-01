use crate::downloader::archive::{ArchiveExtractor, ZipArchiveExtractor};
use crate::downloader::cdn::DepotContext;
use crate::downloader::manifest::cipher::ManifestCipher;
use crate::downloader::manifest::types::{ChunkInfo, ManifestEntry};
use crate::error::{ManifestError, Result, SteamVentError};
use std::sync::Arc;
use steam_vent_proto::content_manifest::{ContentManifestMetadata, ContentManifestPayload};
use steam_vent_proto::protobuf::Message;

const MAGIC_PAYLOAD: u32 = 0x71F617D0;
const MAGIC_METADATA_1: u32 = 0x1F448235;
const MAGIC_METADATA_2: u32 = 0x1F4812BE;
const MAGIC_SIGNATURE_1: u32 = 0x1A670D5A;
const MAGIC_SIGNATURE_2: u32 = 0x1B81B817;

pub(crate) struct RawManifest {
    pub(crate) payload: ContentManifestPayload,
    pub(crate) metadata: Option<ContentManifestMetadata>,
}

impl RawManifest {
    pub(crate) fn parse(payload_bytes: &[u8]) -> Result<Self> {
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
                u32::from_le_bytes(payload_bytes[cursor + 4..cursor + 8].try_into().unwrap())
                    as usize;

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

        let payload = ContentManifestPayload::parse_from_bytes(&final_payload)
            .map_err(|e| SteamVentError::Manifest(ManifestError::Decode(e.to_string())))?;

        Ok(Self { payload, metadata })
    }

    pub(crate) fn filenames_encrypted(&self) -> Result<bool> {
        match &self.metadata {
            Some(m) => m.filenames_encrypted.ok_or_else(|| {
                SteamVentError::Manifest(ManifestError::MissingField(
                    "metadata.filenames_encrypted",
                ))
            }),
            None => Ok(false),
        }
    }

    pub(crate) fn depot_id_for_key(&self, fallback: u32) -> Result<u32> {
        if self.filenames_encrypted()? {
            self.metadata
                .as_ref()
                .and_then(|m| m.depot_id)
                .ok_or_else(|| {
                    SteamVentError::Manifest(ManifestError::MissingField("metadata.depot_id"))
                })
        } else {
            Ok(fallback)
        }
    }

    pub(crate) fn decrypt_filenames(&mut self, depot_key: &[u8]) -> Result<()> {
        let cipher = ManifestCipher::new(depot_key)?;
        cipher.decrypt_filenames(&mut self.payload)
    }

    pub(crate) fn into_entries(self, depot: Arc<DepotContext>) -> Result<Vec<ManifestEntry>> {
        let mut entries = Vec::new();

        for mut mapping in self.payload.mappings {
            let filename = mapping.filename.take().ok_or_else(|| {
                SteamVentError::Manifest(ManifestError::MissingField("mapping.filename"))
            })?;
            let filename = filename.replace('\\', "/");
            let chunks: Vec<ChunkInfo> = mapping
                .chunks
                .into_iter()
                .map(ChunkInfo::try_from)
                .collect::<Result<Vec<_>>>()?;

            entries.push(ManifestEntry {
                filename,
                chunks,
                flags: mapping.flags.take(),
                size: mapping.size.take(),
                depot: Arc::clone(&depot),
            });
        }

        Ok(entries)
    }
}
