use std::collections::HashMap;
use std::path::{Path, PathBuf};

use futures::StreamExt;
use tokio::io::AsyncWriteExt;

use steam_vent_cdn::{DownloadItem, DownloadStream, Result, SteamVentError};

/// State for a single file being written to disk.
struct FileWriteState {
    file_handle: tokio::fs::File,
    file_path: PathBuf,
    bytes_written: u64,
}

/// Consume a [`DownloadStream`] and write every chunk to the filesystem under
/// `base_path`, using the filenames from `files`.
pub async fn write_stream_to_directory(
    stream: &mut DownloadStream,
    files: &[DownloadItem],
    base_path: &Path,
) -> Result<()> {
    let mut file_states: HashMap<usize, FileWriteState> = HashMap::new();

    while let Some(result) = stream.next().await {
        match result {
            Ok(chunk) => {
                let file_index = chunk.file_index;

                // Lazily open the file on first chunk arrival.
                if !file_states.contains_key(&file_index) {
                    let file = &files[file_index];
                    let file_path = base_path.join(&file.filename);

                    if let Some(p) = file_path.parent() {
                        tokio::fs::create_dir_all(p).await?;
                    }

                    let file_handle = tokio::fs::File::create(&file_path).await?;
                    file_states.insert(
                        file_index,
                        FileWriteState {
                            file_handle,
                            file_path,
                            bytes_written: 0,
                        },
                    );
                }

                let state = file_states
                    .get_mut(&file_index)
                    .expect("file state was just inserted");

                state.file_handle.write_all(&chunk.data).await?;
                state.bytes_written += chunk.data.len() as u64;

                if chunk.is_last {
                    state.file_handle.flush().await?;
                    let bytes = state.bytes_written as usize;
                    let path = state.file_path.clone();
                    file_states.remove(&file_index);
                    tracing::info!("Completed file: {} ({} bytes)", path.display(), bytes);
                }
            }
            Err(e) => {
                // Clean up any partially written files before returning.
                for (_, state) in file_states.into_iter() {
                    drop(state.file_handle);
                    if let Err(rm_err) = tokio::fs::remove_file(&state.file_path).await {
                        tracing::warn!(
                            "Failed to remove incomplete file {}: {}",
                            state.file_path.display(),
                            rm_err
                        );
                    }
                }
                tracing::error!("Error in download stream: {}", e);
                return Err(e);
            }
        }
    }

    // Warn about any files that weren't fully written (shouldn't happen if
    // the stream completed normally, but guard against it).
    if !file_states.is_empty() {
        for (idx, state) in &file_states {
            tracing::warn!(
                "File '{}' (index {}) was not completed by the stream",
                state.file_path.display(),
                idx
            );
            if let Err(rm_err) = tokio::fs::remove_file(&state.file_path).await {
                tracing::warn!(
                    "Failed to remove incomplete file {}: {}",
                    state.file_path.display(),
                    rm_err
                );
            }
        }
        return Err(SteamVentError::Io(std::io::Error::other(
            "download stream ended with incomplete files",
        )));
    }

    Ok(())
}
