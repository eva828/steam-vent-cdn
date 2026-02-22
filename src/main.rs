mod archive;
mod cdn;
mod config;
mod connection;
mod downloader;
mod error;
mod manifest;

use clap::Parser;
use downloader::{CdnDownloader, DownloadItem, ChunkDownloadTask, CompressedChunk, DownloadedChunk};
use crate::cdn::CdnClient;
use error::Result;
use indicatif::MultiProgress;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, mpsc, Semaphore};
use tracing_indicatif::IndicatifWriter;
use tracing_indicatif::writer::Stdout;
use tracing_subscriber::Layer;
use tracing_subscriber::fmt::time::UtcTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(help = "The ID of the app that the workshop item belongs to")]
    app_id: u32,

    #[arg(help = "The ID of the workshop item to download")]
    item_id: u64,

    #[arg(
        short,
        long,
        default_value = "./output",
        help = "The directory where the downloaded files will be saved"
    )]
    output: PathBuf,

    #[arg(short, long, help = "Steam username for authentication")]
    username: Option<String>,

    #[arg(
        short,
        long,
        help = "If a username is provided but the password is omitted, you will be prompted for it."
    )]
    password: Option<String>,

    #[arg(
        short,
        long,
        help = "Only list the files in the workshop item without downloading"
    )]
    list: bool,

    #[arg(
        long,
        default_value_t = 16,
        help = "Parallel download connections"
    )]
    parallel_download: usize,

    #[arg(
        long,
        default_value_t = 8,
        help = "Parallel decode workers"
    )]
    parallel_decode: usize,

    #[arg(short, long, help = "Enable verbose output")]
    verbose: bool,
}

fn setup_tracing(verbose: bool) -> (MultiProgress, tracing_appender::non_blocking::WorkerGuard) {
    // Write logs to a file in the current directory, clearing previous contents
    let log_file = std::fs::File::create("run.log").expect("Failed to create log file");
    let (non_blocking, guard) = tracing_appender::non_blocking(log_file);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if verbose {
            tracing_subscriber::EnvFilter::new("steam_vent_cdn=debug,steam_vent=debug,info")
        } else {
            tracing_subscriber::EnvFilter::new("info")
        }
    });

    let file_layer = tracing_subscriber::fmt::layer()
        .with_level(true)
        .with_target(true)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_ansi(false)
        .with_writer(non_blocking)
        .boxed();

    let multi = MultiProgress::new();
    let writer = IndicatifWriter::<Stdout>::new(multi.clone());

    let base_stdout = tracing_subscriber::fmt::layer()
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_ansi(true)
        .with_writer(writer);

    let stdout_layer = if verbose {
        base_stdout
            .with_level(true)
            .with_target(true)
            .with_timer(UtcTime::rfc_3339())
            .boxed()
    } else {
        base_stdout
            .with_level(false)
            .with_target(false)
            .without_time()
            .boxed()
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stdout_layer)
        .with(file_layer)
        .init();

    (multi, guard)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let (multi, _guard) = setup_tracing(args.verbose);

    tracing::debug!("Item ID: {}", args.item_id);

    let credentials = crate::config::Credentials::from_options(args.username, args.password);
    let config = crate::config::DownloaderConfig::new(credentials)
        .with_app_id(args.app_id)
        .with_parallel_download(args.parallel_download)
        .with_parallel_decode(args.parallel_decode);

    let steam_client = crate::connection::SteamClient::new(config);

    let steam_session = steam_client.connect().await?;

    let mut downloader = CdnDownloader::new(steam_session);

    let manifest_files = downloader.fetch_manifest_info(args.item_id).await?;

    let items: Vec<DownloadItem> = manifest_files
        .into_iter()
        .map(|(filename, manifest_info)| DownloadItem {
            item_id: args.item_id,
            filename,
            manifest_info,
        })
        .collect();

    const FLAG_DIRECTORY: u32 = 0x40;

    let mut directories: Vec<DownloadItem> = Vec::new();
    let mut files: Vec<DownloadItem> = Vec::new();

    for item in items {
        if item.manifest_info.flags.unwrap_or(0) & FLAG_DIRECTORY != 0 {
            directories.push(item);
        } else {
            files.push(item);
        }
    }

    tracing::info!(
        "Found {} directories and {} files to download.",
        directories.len(),
        files.len()
    );

    if args.list {
        tracing::info!("Workshop item {} files:", args.item_id);
        for file in &files {
            let size = file.manifest_info.size.unwrap_or_else(|| {
                file.manifest_info.chunks
                    .iter()
                    .map(|c| c.uncompressed_size as u64)
                    .sum()
            });

            let size_str = if size < 1024 {
                format!("{} B", size)
            } else if size < 1024 * 1024 {
                format!("{:.2} KB", size as f64 / 1024.0)
            } else {
                format!("{:.2} MB", size as f64 / (1024.0 * 1024.0))
            };

            tracing::info!("  - {} ({})", file.filename, size_str);
        }

        tracing::info!("Total: {} files", files.len());
        return Ok(());
    }

    for dir in &directories {
        let output_path = args
            .output
            .join(args.app_id.to_string())
            .join(args.item_id.to_string())
            .join(&dir.filename);

        tokio::fs::create_dir_all(&output_path).await?;
    }

    let total_compressed: u64 = files
        .iter()
        .map(|f| {
            f.manifest_info.chunks
                .iter()
                .map(|c| c.compressed_size as u64)
                .sum::<u64>()
        })
        .sum();

    let total_decompressed: u64 = files
        .iter()
        .map(|f| {
            f.manifest_info.chunks
                .iter()
                .map(|c| c.uncompressed_size as u64)
                .sum::<u64>()
        })
        .sum();

    tracing::info!(
        "Total sizes: {} compressed ({} MiB), {} decompressed ({} MiB)",
        total_compressed,
        total_compressed / 1024 / 1024,
        total_decompressed,
        total_decompressed / 1024 / 1024
    );

    let pb_download = multi.add(indicatif::ProgressBar::new(total_compressed));
    pb_download.set_style(
        indicatif::ProgressStyle::with_template(
            "{spinner:.blue} Downloading [{elapsed_precise}] [{wide_bar:.blue/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})",
        )
        .unwrap()
        .progress_chars("#>-"),
    );
    pb_download.enable_steady_tick(std::time::Duration::from_millis(100));

    let pb_decode = multi.add(indicatif::ProgressBar::new(total_decompressed));
    pb_decode.set_style(
        indicatif::ProgressStyle::with_template(
            "{spinner:.green} Decoding [{elapsed_precise}] [{wide_bar:.green/green}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})",
        )
        .unwrap()
        .progress_chars("#>-"),
    );
    pb_decode.enable_steady_tick(std::time::Duration::from_millis(100));

    let pb_write = multi.add(indicatif::ProgressBar::new(total_decompressed));
    pb_write.set_style(
        indicatif::ProgressStyle::with_template(
            "{spinner:.yellow} Writing [{elapsed_precise}] [{wide_bar:.yellow/yellow}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})",
        )
        .unwrap()
        .progress_chars("#>-"),
    );
    pb_write.enable_steady_tick(std::time::Duration::from_millis(100));

    for file in &files {
        let output_path = args
            .output
            .join(args.app_id.to_string())
            .join(args.item_id.to_string())
            .join(&file.filename);

        if let Some(parent) = output_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }

    let start_time = std::time::Instant::now();
    let mut downloaded_bytes: u64 = 0;
    let mut chunk_count: u64 = 0;

    let base_path = args
        .output
        .join(args.app_id.to_string())
        .join(args.item_id.to_string());

    let mut tasks: Vec<ChunkDownloadTask> = Vec::new();
    for (file_index, file) in files.iter().enumerate() {
        let depot = Arc::clone(&file.manifest_info.depot);
        for (chunk_index, chunk) in file.manifest_info.chunks.iter().enumerate() {
            tasks.push(ChunkDownloadTask {
                chunk_id: hex::encode(&chunk.sha),
                depot: Arc::clone(&depot),
                file_index,
                chunk_index,
                compressed_size: chunk.compressed_size,
            });
        }
    }

    let download_sem = Arc::new(Semaphore::new(args.parallel_download));
    let decode_sem = Arc::new(Semaphore::new(args.parallel_decode));

    let (download_tx, download_rx) = mpsc::channel::<Result<CompressedChunk>>(32);
    let (decode_tx, mut decode_rx) = mpsc::channel::<Result<DownloadedChunk>>(32);

    let cdn_client = downloader.cdn_client.clone();

    for task in tasks {
        let download_sem = download_sem.clone();
        let download_tx = download_tx.clone();
        let cdn_client = cdn_client.clone();
        let pb_download = pb_download.clone();

        tokio::spawn(async move {
            let _permit = download_sem.acquire().await.unwrap();
            let compressed_data = cdn_client
                .download_chunk(
                    &task.depot.server_hosts,
                    task.depot.depot_id,
                    &task.chunk_id,
                )
                .await;

            match compressed_data {
                Ok(data) => {
                    pb_download.inc(task.compressed_size as u64);
                    let _ = download_tx.send(Ok(CompressedChunk {
                        depot: task.depot,
                        file_index: task.file_index,
                        chunk_index: task.chunk_index,
                        compressed_data: data,
                    })).await;
                }
                Err(e) => {
                    let _ = download_tx.send(Err(e)).await;
                }
            }
        });
    }

    let download_rx = Arc::new(Mutex::new(download_rx));
    let decode_sem_clone = decode_sem.clone();
    let decode_tx_clone = decode_tx.clone();
    let pb_decode_clone = pb_decode.clone();

    tokio::spawn(async move {
        loop {
            let mut rx = download_rx.lock().await;
            match rx.recv().await {
                Some(Ok(compressed)) => {
                    drop(rx);
                    let _permit = decode_sem_clone.acquire().await.unwrap();
                    let decode_tx = decode_tx_clone.clone();
                    let pb_decode = pb_decode_clone.clone();

                    tokio::spawn(async move {
                        let depot_key = compressed.depot.depot_key.as_deref();
                        let decoded: Result<Vec<u8>> = CdnClient::decode_chunk(&compressed.compressed_data, depot_key);

                        match decoded {
                            Ok(data) => {
                                pb_decode.inc(data.len() as u64);
                                let _ = decode_tx.send(Ok(DownloadedChunk {
                                    file_index: compressed.file_index,
                                    chunk_index: compressed.chunk_index,
                                    data,
                                })).await;
                            }
                            Err(e) => {
                                let _ = decode_tx.send(Err(e)).await;
                            }
                        }
                    });
                }
                Some(Err(e)) => {
                    let _ = decode_tx.send(Err(e)).await;
                }
                None => break,
            }
        }
    });

    drop(download_tx);

    let chunk_buffers: Arc<Mutex<HashMap<usize, Vec<(usize, Vec<u8>)>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    while let Some(result) = decode_rx.recv().await {
        match result {
            Ok(chunk) => {
                pb_write.inc(chunk.data.len() as u64);
                downloaded_bytes += chunk.data.len() as u64;
                chunk_count += 1;

                let mut buffers = chunk_buffers.lock().await;
                buffers
                    .entry(chunk.file_index)
                    .or_insert_with(Vec::new)
                    .push((chunk.chunk_index, chunk.data));

                let file = &files[chunk.file_index];
                let total_chunks = file.manifest_info.chunks.len();

                if chunk_count % 100 == 0 {
                    let elapsed = start_time.elapsed();
                    let speed = if elapsed.as_secs() > 0 {
                        downloaded_bytes / elapsed.as_secs()
                    } else {
                        downloaded_bytes
                    };
                    tracing::debug!(
                        "Progress: {} chunks ({} bytes), speed: {} bytes/sec",
                        chunk_count,
                        downloaded_bytes,
                        speed
                    );
                }

                if let Some(chunks) = buffers.get(&chunk.file_index) {
                    if chunks.len() == total_chunks {
                        let file_path = base_path.join(&file.filename);
                        let mut file_data: Vec<(usize, Vec<u8>)> = chunks.clone();
                        file_data.sort_by_key(|(idx, _)| *idx);

                        let mut full_data = Vec::new();
                        for (_, data) in file_data {
                            full_data.extend(data);
                        }

                        buffers.remove(&chunk.file_index);

                        let output_path = file_path.clone();
                        tokio::spawn(async move {
                            let mut file_handle = File::create(&output_path).await.unwrap();
                            file_handle.write_all(&full_data).await.unwrap();
                            tracing::info!("Completed file: {} ({} bytes)", output_path.display(), full_data.len());
                        });
                    }
                }
            }
            Err(e) => {
                tracing::error!("Error: {}", e);
            }
        }
    }

    for (file_index, buffers) in chunk_buffers.lock().await.iter() {
        if !buffers.is_empty() {
            let file = &files[*file_index];
            let file_path = base_path.join(&file.filename);
            let mut file_data: Vec<(usize, Vec<u8>)> = buffers.clone();
            file_data.sort_by_key(|(idx, _)| *idx);

            let mut full_data = Vec::new();
            for (_, data) in file_data {
                full_data.extend(data);
            }

            let mut file_handle = File::create(&file_path).await?;
            file_handle.write_all(&full_data).await?;
            tracing::info!("Completed file: {} ({} bytes)", file_path.display(), full_data.len());
        }
    }

    pb_download.finish_with_message("Download complete");
    pb_decode.finish_with_message("Decode complete");
    pb_write.finish_with_message("Write complete");

    let elapsed = start_time.elapsed();
    let speed = if elapsed.as_secs() > 0 {
        downloaded_bytes / elapsed.as_secs()
    } else {
        downloaded_bytes
    };
    tracing::info!(
        "Download complete: {} bytes, {} chunks, {} seconds, {} bytes/sec",
        downloaded_bytes,
        chunk_count,
        elapsed.as_secs_f32(),
        speed
    );

    Ok(())
}
