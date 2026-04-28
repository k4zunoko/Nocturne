use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::Command;

use rodio::Decoder;
use stream_download::storage::temp::TempStorageProvider;
use stream_download::{Settings, StreamDownload};
use tokio::runtime::Builder as RuntimeBuilder;

const YT_DLP_AUDIO_FORMAT: &str =
    "140/139/bestaudio[ext=m4a]/bestaudio[acodec^=mp4a]/bestaudio[ext=mp3]/bestaudio[acodec^=mp3]/best[ext=mp4]/best";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source_url = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";
    let playback_url = resolve_youtube_playback_url(source_url)?;
    println!("resolved_url={playback_url}");

    let runtime = RuntimeBuilder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    let mut inspect_reader = runtime.block_on(open_stream(&playback_url))?;
    println!("content_length={:?}", inspect_reader.content_length());
    let mut prefix = [0_u8; 32];
    inspect_reader.read_exact(&mut prefix)?;
    inspect_reader.seek(SeekFrom::Start(0))?;
    println!("prefix_hex={}", hex_string(&prefix));

    let decoder_reader = runtime.block_on(open_stream(&playback_url))?;
    match Decoder::new(decoder_reader) {
        Ok(_) => println!("decoder_new=ok"),
        Err(error) => println!("decoder_new=err:{error}"),
    }

    let builder_reader = runtime.block_on(open_stream(&playback_url))?;
    let byte_len = builder_reader.content_length();
    let mut builder = Decoder::builder().with_data(builder_reader);
    if let Some(byte_len) = byte_len {
        builder = builder.with_byte_len(byte_len);
    }
    match builder.build() {
        Ok(_) => println!("decoder_builder=ok"),
        Err(error) => println!("decoder_builder=err:{error}"),
    }

    let mut file_reader = runtime.block_on(open_stream(&playback_url))?;
    let mut full_bytes = Vec::new();
    file_reader.read_to_end(&mut full_bytes)?;
    let temp_path = std::env::temp_dir().join("nocturne-repro-audio.webm");
    let mut file = File::create(&temp_path)?;
    file.write_all(&full_bytes)?;
    drop(file);
    let file = File::open(&temp_path)?;
    match Decoder::new(file) {
        Ok(_) => println!("decoder_file=ok"),
        Err(error) => println!("decoder_file=err:{error}"),
    }

    Ok(())
}

async fn open_stream(
    playback_url: &str,
) -> Result<StreamDownload<TempStorageProvider>, Box<dyn std::error::Error>> {
    Ok(StreamDownload::new_http(
        playback_url.parse()?,
        TempStorageProvider::new(),
        Settings::default(),
    )
    .await?)
}

fn resolve_youtube_playback_url(source_url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new(yt_dlp_executable())
        .args([
            "--get-url",
            "--format",
            YT_DLP_AUDIO_FORMAT,
            "--no-playlist",
            "--no-warnings",
            "--quiet",
            source_url,
        ])
        .output()?;

    if !output.status.success() {
        return Err(format!("yt-dlp failed: {}", String::from_utf8_lossy(&output.stderr)).into());
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| String::from("yt-dlp returned no playback URL").into())
}

fn yt_dlp_executable() -> std::ffi::OsString {
    std::env::var_os("NOCTURNE_YT_DLP_PATH")
        .or_else(|| {
            let windows_fallback = std::path::PathBuf::from(r"C:\tools\yt-dlp\yt-dlp.exe");
            windows_fallback.is_file().then(|| windows_fallback.into_os_string())
        })
        .unwrap_or_else(|| "yt-dlp".into())
}

fn hex_string(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
