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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerMode;
    use crate::downloader::cdn::servers::{ContentServerList, DepotContext};
    use crate::error::SteamVentError;
    use std::sync::Arc;
    use steam_vent_proto::content_manifest::content_manifest_payload::FileMapping;
    use steam_vent_proto::content_manifest::content_manifest_payload::file_mapping::ChunkData as ProtoChunk;
    use steam_vent_proto::content_manifest::{ContentManifestMetadata, ContentManifestPayload};

    fn make_payload_with_files(files: &[(&str, u64)]) -> Vec<u8> {
        let mut payload = ContentManifestPayload::new();
        for &(name, size) in files {
            let mut chunk = ProtoChunk::new();
            chunk.sha = Some(vec![0xABu8; 20]);
            chunk.crc = Some(42);
            chunk.offset = Some(0);
            chunk.cb_original = Some(size as u32);
            chunk.cb_compressed = Some(size as u32);

            let mut mapping = FileMapping::new();
            mapping.filename = Some(name.to_string());
            mapping.chunks = vec![chunk].into();
            mapping.flags = Some(0);
            mapping.size = Some(size);

            payload.mappings.push(mapping);
        }
        payload.write_to_bytes().unwrap()
    }

    fn make_segment(magic: u32, data: &[u8]) -> Vec<u8> {
        let mut seg = Vec::new();
        seg.extend_from_slice(&magic.to_le_bytes());
        seg.extend_from_slice(&(data.len() as u32).to_le_bytes());
        seg.extend_from_slice(data);
        seg
    }

    fn make_manifest_zip(segments: &[&[u8]]) -> Vec<u8> {
        use std::io::Write;
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("m", opts).unwrap();
        for seg in segments {
            zip.write_all(seg).unwrap();
        }
        let finished = zip.finish().unwrap();
        finished.into_inner()
    }

    fn make_empty_depot() -> Arc<DepotContext> {
        Arc::new(DepotContext {
            servers: ContentServerList::from_servers(vec![], ServerMode::Cdn),
            depot_id: 9999,
            depot_key: None,
        })
    }

    #[test]
    fn decode_valid_manifest_with_payload_and_metadata() {
        let payload_data = make_payload_with_files(&[("data/game.cfg", 2048)]);
        let payload_seg = make_segment(MAGIC_PAYLOAD, &payload_data);

        let mut metadata = ContentManifestMetadata::new();
        metadata.filenames_encrypted = Some(false);
        metadata.depot_id = Some(4321);
        let metadata_data = metadata.write_to_bytes().unwrap();
        let metadata_seg = make_segment(MAGIC_METADATA_1, &metadata_data);

        let zip = make_manifest_zip(&[&payload_seg, &metadata_seg]);

        let raw = RawManifest::decode(&zip).unwrap();
        assert_eq!(raw.filenames_encrypted().unwrap(), false);
        assert_eq!(raw.depot_id_for_key(1111).unwrap(), 1111);

        let entries = raw.into_entries(make_empty_depot()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].filename, "data/game.cfg");
        assert_eq!(entries[0].size, Some(2048));
    }

    #[test]
    fn decode_fails_on_missing_payload() {
        let mut metadata = ContentManifestMetadata::new();
        metadata.filenames_encrypted = Some(true);
        metadata.depot_id = Some(1234);
        let metadata_data = metadata.write_to_bytes().unwrap();
        let metadata_seg = make_segment(MAGIC_METADATA_1, &metadata_data);
        let zip = make_manifest_zip(&[&metadata_seg]);

        assert!(matches!(
            RawManifest::decode(&zip),
            Err(SteamVentError::Manifest(ManifestError::MissingPayload))
        ));
    }

    #[test]
    fn decode_fails_on_unknown_magic() {
        let bad_seg = make_segment(0xDEADBEEF, b"junk");
        let zip = make_manifest_zip(&[&bad_seg]);

        assert!(matches!(
            RawManifest::decode(&zip),
            Err(SteamVentError::Manifest(
                ManifestError::UnknownSegmentMagic(0xDEADBEEF)
            ))
        ));
    }

    #[test]
    fn decode_fails_on_segment_length_exceeded() {
        let payload_data = make_payload_with_files(&[("x", 100)]);
        // Lie about segment length: claim more data than available
        let mut seg = Vec::new();
        seg.extend_from_slice(&MAGIC_PAYLOAD.to_le_bytes());
        seg.extend_from_slice(&(payload_data.len() as u32 + 9999).to_le_bytes());
        seg.extend_from_slice(&payload_data);
        let zip = make_manifest_zip(&[&seg]);

        assert!(matches!(
            RawManifest::decode(&zip),
            Err(SteamVentError::Manifest(
                ManifestError::SegmentLengthExceeded { .. }
            ))
        ));
    }

    #[test]
    fn into_entries_fails_on_missing_filename() {
        let payload_data = make_payload_with_files(&[("ok", 10)]);
        let payload_seg = make_segment(MAGIC_PAYLOAD, &payload_data);
        let zip = make_manifest_zip(&[&payload_seg]);

        let mut raw = RawManifest::decode(&zip).unwrap();
        // Nuke the filename to trigger error
        raw.payload.mappings[0].filename = None;

        assert!(raw.into_entries(make_empty_depot()).is_err());
    }

    #[test]
    fn into_entries_fails_on_missing_chunk_sha() {
        let payload_data = make_payload_with_files(&[("broken", 50)]);
        let payload_seg = make_segment(MAGIC_PAYLOAD, &payload_data);
        let zip = make_manifest_zip(&[&payload_seg]);

        let mut raw = RawManifest::decode(&zip).unwrap();
        // Remove sha from chunk
        raw.payload.mappings[0].chunks[0].sha = None;

        assert!(raw.into_entries(make_empty_depot()).is_err());
    }
}
