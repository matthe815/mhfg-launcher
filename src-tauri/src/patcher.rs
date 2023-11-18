use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use log::{info, warn};
use serde::Serialize;
use serde_repr::Serialize_repr;
use sha2::Digest;
use tauri::Window;
use tokio::select;
use tokio_util::sync::CancellationToken;

use crate::{server::PatcherResponse, LogPayload};

const ETAG_FILE: &str = "patcher.etag";

#[derive(Debug, Serialize_repr, Clone)]
#[repr(u8)]
enum State {
    Checking,
    Downloading,
    Patching,
    Done,
    Error,
}

#[derive(Debug, Clone, Serialize)]
struct PatcherEvent {
    total: usize,
    current: usize,
    state: State,
}

fn send_event(window: &Window, total: usize, current: usize, state: State) {
    window
        .emit(
            "patcher",
            PatcherEvent {
                total,
                current,
                state,
            },
        )
        .unwrap_or_else(|e| warn!("failed to emit message: {}", e));
}

fn send_error(window: &Window, msg: &str) {
    warn!("patcher error: {}", msg);
    window
        .emit("log", LogPayload::error(msg))
        .unwrap_or_else(|e| warn!("failed to emit message: {}", e));
    window
        .emit(
            "patcher",
            PatcherEvent {
                total: 0,
                current: 0,
                state: State::Error,
            },
        )
        .unwrap_or_else(|e| warn!("failed to emit message: {}", e));
}

fn get_changed_paths<'a>(
    patcher_content: &'a str,
    game_folder: &Path,
) -> Result<Vec<&'a str>, &'static str> {
    patcher_content
        .lines()
        .filter_map(|line| {
            let Some((patcher_hash, mut patcher_path)) = line.split_once('\t') else {
                return Some(Err("Patcher server returned invalid data"));
            };
            patcher_path = patcher_path.trim_start_matches('/');
            let client_path = game_folder.join(patcher_path);

            info!(
                "files: {} {} {}",
                game_folder.to_str().unwrap(),
                &patcher_path,
                &client_path.to_str().unwrap()
            );

            if let Ok(mut file) = fs::File::open(&client_path) {
                let mut hasher = sha2::Sha256::new();
                if io::copy(&mut file, &mut hasher).is_ok() {
                    let client_hash = format!("{:x}", hasher.finalize());
                    info!("hashes: {} {}", patcher_hash, client_hash);
                    if patcher_hash == client_hash {
                        return None;
                    }
                };
            };
            Some(Ok(patcher_path))
        })
        .try_collect()
        .or(Err("Failed to parse patcher server response"))
}

async fn download_changed_paths(
    window: &Window,
    client: &reqwest::Client,
    patcher_url: &str,
    changed_paths: &[&str],
    patcher_folder: &Path,
    cancel: CancellationToken,
) -> Result<(), &'static str> {
    let total = changed_paths.len();
    let mut current = 0;
    for changed_path in changed_paths {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let req = client
            .get(format!("{}/{}", patcher_url, changed_path))
            .send();
        let mut resp = select! {
            _ = cancel.cancelled() => return Ok(()),
            resp = req => resp.or(Err("Patch server request failed"))?,
        };
        let patcher_path = patcher_folder.join(changed_path);
        fs::create_dir_all(
            patcher_path
                .parent()
                .ok_or("Failed to get temp file parent")?,
        )
        .or(Err("Failed to create temp file parent"))?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(patcher_path)
            .or(Err("Failed to open temp file"))?;
        while let Some(chunk) = select! {
            _ = cancel.cancelled() => return Ok(()),
            chunk = resp.chunk() => chunk.or(Err("Failed to read patcher response chunk"))?
        } {
            file.write_all(&chunk)
                .or(Err("Failed to write patcher response chunk to file"))?;
        }
        current += 1;
        send_event(window, total, current, State::Downloading);
    }
    Ok(())
}

fn move_changed_paths(
    changed_paths: &[&str],
    source_folder: &Path,
    target_folder: &Path,
) -> Result<(), &'static str> {
    for path in changed_paths {
        let source_path = source_folder.join(path);
        let target_path = target_folder.join(path);
        fs::create_dir_all(
            target_path
                .parent()
                .ok_or("Failed to get target file parent")?,
        )
        .or(Err("Failed to create target file parent"))?;
        fs::rename(&source_path, &target_path)
            .or(Err("Failed to move patched file to game folder"))?;
    }
    Ok(())
}

async fn patch_internal(
    window: &Window,
    client: reqwest::Client,
    patcher_url: String,
    patcher_resp: PatcherResponse,
    game_folder: &Path,
    patcher_folder: &Path,
    cancel: CancellationToken,
) -> Result<(), &'static str> {
    send_event(window, 0, 0, State::Checking);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let changed_paths = get_changed_paths(&patcher_resp.content, game_folder)?;
    send_event(window, changed_paths.len(), 0, State::Downloading);
    tokio::time::sleep(Duration::from_secs(1)).await;
    download_changed_paths(
        window,
        &client,
        &patcher_url,
        &changed_paths,
        patcher_folder,
        cancel,
    )
    .await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    send_event(window, 0, 0, State::Patching);
    move_changed_paths(&changed_paths, patcher_folder, game_folder)?;
    set_etag(game_folder, &patcher_resp.etag)?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    send_event(window, 0, 0, State::Done);
    Ok(())
}

pub async fn patch(
    window: Window,
    client: reqwest::Client,
    patcher_url: String,
    patcher_resp: PatcherResponse,
    game_folder: PathBuf,
    cancel: CancellationToken,
) {
    let tmp_folder = game_folder.join("tmp");
    if let Err(e) = fs::create_dir_all(&tmp_folder) {
        warn!("error creating patcher dir: {}", e);
        send_error(&window, "Failed to create temp patcher directory");
        return;
    }
    if let Err(error) = patch_internal(
        &window,
        client,
        patcher_url,
        patcher_resp,
        &game_folder,
        &tmp_folder,
        cancel,
    )
    .await
    {
        send_error(&window, error);
    }
    if let Err(e) = fs::remove_dir_all(&tmp_folder) {
        warn!("error deleting patcher dir: {}", e);
        send_error(&window, "Failed to delete temp patcher directory");
    }
}

fn set_etag(game_folder: &Path, etag: &str) -> Result<(), &'static str> {
    fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .create(true)
        .open(game_folder.join(ETAG_FILE))
        .or(Err("Failed to open patcher etag file"))?
        .write_all(etag.as_bytes())
        .or(Err("Failed to write to patcher etag file"))?;
    Ok(())
}

pub fn get_etag(game_folder: &Path) -> String {
    let Ok(etag) = fs::read_to_string(game_folder.join(ETAG_FILE)) else {
        return "".into();
    };
    etag
}
