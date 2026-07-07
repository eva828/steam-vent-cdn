use crate::downloader::archive::{ArchiveExtractor, ZipArchiveExtractor};
use crate::downloader::cdn::DepotContext;
use crate::downloader::manifest::cipher::ManifestCipher;
use crate::downloader::manifest::types::{ChunkInfo, ManifestEntry};
use crate::error::{ManifestError, Result};
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
    pub(crate) fn decode(payload_bytes: &[u8]) -> Result<Self> {
        let payload_bytes_vec = ZipArchiveExtractor::new(payload_bytes)?.extract()?;
        let mut payload_bytes = payload_bytes_vec.as_slice();

        let mut final_payload = Vec::new();
        let mut metadata: Option<ContentManifestMetadata> = None;

        fn pop_segment_header(data: &[u8]) -> Option<(u32, usize, &[u8])> {
            let (magic_bytes, rest) = data.split_first_chunk::<4>()?;
            let (length_bytes, rest) = rest.split_first_chunk::<4>()?;

            let magic = u32::from_le_bytes(*magic_bytes);
            let length = u32::from_le_bytes(*length_bytes) as usize;

            Some((magic, length, rest))
        }

        while let Some((magic, length, data)) = pop_segment_header(payload_bytes) {
            tracing::debug!(
                "Found manifest segment: Magic 0x{:08X}, Length {}",
                magic,
                length
            );

            if length > data.len() {
                tracing::error!(
                    "Segment length {} exceeds remaining data {}",
                    length,
                    data.len()
                );
                return Err(ManifestError::SegmentLengthExceeded {
                    length,
                    remaining: data.len(),
                })?;
            }

            let (segment_data, rest) = data.split_at(length);

            if magic == MAGIC_PAYLOAD {
                tracing::debug!("Processing Payload segment");
                final_payload.extend_from_slice(segment_data);
            } else if magic == MAGIC_METADATA_1 || magic == MAGIC_METADATA_2 {
                tracing::debug!("Decoding Metadata segment");
                let m = ContentManifestMetadata::parse_from_bytes(segment_data)
                    .map_err(|e| ManifestError::Decode(e.to_string()))?;
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
                return Err(ManifestError::UnknownSegmentMagic(magic))?;
            }

            payload_bytes = rest;
        }

        if final_payload.is_empty() {
            tracing::error!("No Payload segment found, manifest is invalid");
            return Err(ManifestError::MissingPayload)?;
        }

        let payload = ContentManifestPayload::parse_from_bytes(&final_payload)
            .map_err(|e| ManifestError::Decode(e.to_string()))?;

        Ok(Self { payload, metadata })
    }

    pub(crate) fn filenames_encrypted(&self) -> Result<bool> {
        match &self.metadata {
            Some(m) => Ok(m
                .filenames_encrypted
                .ok_or(ManifestError::MissingField("metadata.filenames_encrypted"))?),
            None => Ok(false),
        }
    }

    pub(crate) fn depot_id_for_key(&self, fallback: u32) -> Result<u32> {
        if self.filenames_encrypted()? {
            Ok(self
                .metadata
                .as_ref()
                .and_then(|m| m.depot_id)
                .ok_or(ManifestError::MissingField("metadata.depot_id"))?)
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
            let filename = mapping
                .filename
                .take()
                .ok_or(ManifestError::MissingField("mapping.filename"))?;
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
