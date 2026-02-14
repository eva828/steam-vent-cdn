use prost::Message;

#[derive(Clone, PartialEq, Message)]
pub struct ContentManifestPayload {
    #[prost(message, repeated, tag = "1")]
    pub mappings: Vec<FileMapping>,
}

#[derive(Clone, PartialEq, Message)]
pub struct FileMapping {
    #[prost(string, optional, tag = "1")]
    pub filename: Option<String>,
    #[prost(uint64, optional, tag = "2")]
    pub size: Option<u64>,
    #[prost(uint32, optional, tag = "3")]
    pub flags: Option<u32>,
    #[prost(bytes, optional, tag = "4")]
    pub sha_filename: Option<Vec<u8>>,
    #[prost(bytes, optional, tag = "5")]
    pub sha_content: Option<Vec<u8>>,
    #[prost(message, repeated, tag = "6")]
    pub chunks: Vec<ChunkData>,
    #[prost(string, optional, tag = "7")]
    pub linktarget: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChunkData {
    #[prost(bytes, optional, tag = "1")]
    pub sha: Option<Vec<u8>>,
    #[prost(fixed32, optional, tag = "2")]
    pub crc: Option<u32>,
    #[prost(uint64, optional, tag = "3")]
    pub offset: Option<u64>,
    #[prost(uint32, optional, tag = "4")]
    pub cb_original: Option<u32>,
    #[prost(uint32, optional, tag = "5")]
    pub cb_compressed: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ContentManifestMetadata {
    #[prost(uint32, optional, tag = "1")]
    pub depot_id: Option<u32>,
    #[prost(uint64, optional, tag = "2")]
    pub gid_manifest: Option<u64>,
    #[prost(uint32, optional, tag = "3")]
    pub creation_time: Option<u32>,
    #[prost(bool, optional, tag = "4")]
    pub filenames_encrypted: Option<bool>,
    #[prost(uint64, optional, tag = "5")]
    pub cb_disk_original: Option<u64>,
    #[prost(uint64, optional, tag = "6")]
    pub cb_disk_compressed: Option<u64>,
    #[prost(uint32, optional, tag = "7")]
    pub unique_chunks: Option<u32>,
    #[prost(uint32, optional, tag = "8")]
    pub crc_encrypted: Option<u32>,
    #[prost(uint32, optional, tag = "9")]
    pub crc_clear: Option<u32>,
}
