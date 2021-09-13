use anyhow::{anyhow, ensure, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use sha1::Digest;
use std::{collections::HashMap, path::PathBuf, time::Duration};
use tempfile::NamedTempFile;
use tokio::{io::AsyncWriteExt, process, sync, task};
use tokio_stream::{self as stream, StreamExt};

use crate::ServerConfig;

const MANIFEST_URL: &str = "https://launchermeta.mojang.com/mc/game/version_manifest.json";

pub struct ServerTask {
    pub join_handle: task::JoinHandle<()>,
    pub should_run: sync::watch::Sender<bool>,
}

pub fn spawn_server_task(config: ServerConfig) -> ServerTask {
    let (tx, rx) = sync::watch::channel(false);

    ServerTask {
        join_handle: tokio::spawn(run_server_task(config, rx)),
        should_run: tx,
    }
}

#[allow(unused_parens)]
async fn run_server_task(config: ServerConfig, should_run: sync::watch::Receiver<bool>) {
    let mut running = false;
    let mut should_run = stream::wrappers::WatchStream::new(should_run).fuse();

    loop {
        // Wait for a startup signal
        tokio::select! { biased;
            message = should_run.next() => match message {
                Some(v) => running = v,
                None => break
            },
            // If the server crashed, relaunch after 10 seconds.
            _ = tokio::time::sleep(Duration::from_secs(10)), if running => {}
        }
        if !running {
            continue;
        }

        log::info!("Starting server...");

        // Start a server
        let jar = match download_if_needed(&config.version).await {
            Ok(path) => path,
            Err(error) => {
                log::error!("Failed to download server: {:?}", error);
                continue;
            }
        };

        let spawn_result = process::Command::new("java")
            .args(
                config
                    .jvm_arguments
                    .iter()
                    .map(|arg| std::ffi::OsStr::new(arg)),
            )
            .arg("-jar")
            .arg(jar.as_os_str())
            .current_dir("server")
            .spawn()
            .context("Could not launch JVM");

        let mut child = match spawn_result {
            Ok(child) => child,
            Err(error) => {
                log::error!("Failed to start server: {:?}", error);
                continue;
            }
        };

        // Wait either for us to recieve a stop signal or for the server to terminate on its own
        let exit_status = tokio::select! {
            (None | Some(false)) = should_run.next() => {
                log::info!("Stopping server...");
                running = false;
                // We received a stop signal, so send a SIGINT to the server.
                if let Some(pid) = child.id() {
                    unsafe { libc::kill(pid as i32, libc::SIGINT); }
                    child.wait().await.ok()
                } else {
                    None
                }
            }
            status = child.wait() => { status.ok() }
        };

        if let Some(status) = exit_status {
            log::info!("Server terminated ({}).", status);
        } else {
            log::info!("Server terminated.")
        }
    }
}

async fn download_if_needed(id: &str) -> Result<PathBuf> {
    let mut client = Client::default();
    let download = find_best_version(&mut client, id)
        .await
        .context("Could not get manifests from server")?;
    let filename = hex::encode(&download.sha1);

    // jars/<sha1>.jar
    let mut target = tokio::fs::canonicalize(".").await?;
    target.extend(["jars", &filename].iter());
    target.set_extension("jar");

    // check if the file exists, and its size matches what we expect
    let valid = match tokio::fs::metadata(&target).await {
        Ok(metadata) => metadata.len() == download.size,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            return Err(e).context(format!("Could not read metadata for {}", target.display()))
        }
    };

    if valid {
        log::debug!("{} already exists, skipping download.", target.display());
        return Ok(target);
    }

    // Download the jar to a temporary file
    log::debug!("Downloading {}...", download.url);
    let mut response = client
        .get(download.url)
        .send()
        .await
        .context("Could not download server jar")?;
    ensure!(
        response.status().is_success(),
        "Server returned error code {}",
        response.status()
    );

    let tmpfile = NamedTempFile::new_in("jars").context("Could not write server jar")?;
    let mut writer = tokio::io::BufWriter::new(tokio::fs::File::from_std(tmpfile.reopen()?));
    let mut hasher = sha1::Sha1::default();

    while let Some(chunk) = response
        .chunk()
        .await
        .context("Could not download server jar")?
    {
        writer
            .write(&chunk)
            .await
            .context("Could not write server jar")?;
        hasher.update(&chunk);
    }

    let hash = hasher.finalize();
    ensure!(
        hash.as_slice() == download.sha1,
        "Hash verification failed! Expected {}, got {}",
        hex::encode(download.sha1),
        hex::encode(hash)
    );

    writer.flush().await.context("Could not write server jar")?;
    tmpfile
        .persist(&target)
        .context("Could not write server jar")?;

    log::debug!("Saved to {}", target.display());
    Ok(target)
}

async fn find_best_version(client: &mut Client, id: &str) -> Result<Download> {
    log::debug!("Downloading {}...", MANIFEST_URL);
    let manifest: Manifest = client.get(MANIFEST_URL).send().await?.json().await?;

    let version = manifest.resolve(id)?;
    log::debug!(
        "Resolved version {}. Downloading {}...",
        version.id,
        version.url,
    );
    let mut version_info: Version = client.get(version.url.clone()).send().await?.json().await?;

    version_info
        .downloads
        .remove(&DownloadType::Server)
        .ok_or_else(|| anyhow!("missing server download"))
}

#[derive(Deserialize)]
struct Manifest {
    latest: HashMap<String, String>,
    versions: Vec<ManifestVersion>,
}

impl Manifest {
    fn resolve(&self, version: &str) -> Result<&ManifestVersion> {
        let version = if let Some(v) = self.latest.get(version) {
            v
        } else {
            version
        };

        self.versions
            .iter()
            .find(|v| v.id == version)
            .ok_or_else(|| anyhow!("Version {} is missing from the manifest", version))
    }
}

#[derive(Deserialize)]
struct ManifestVersion {
    id: String,
    url: String,
}

#[derive(Deserialize)]
struct Version {
    downloads: HashMap<DownloadType, Download>,
}

#[derive(Deserialize, Copy, Clone, Hash, Eq, PartialEq)]
enum DownloadType {
    #[serde(rename = "client")]
    Client,

    #[serde(rename = "client_mappings")]
    ClientMappings,

    #[serde(rename = "server")]
    Server,

    #[serde(rename = "server_mappings")]
    ServerMappings,
}

#[derive(Deserialize)]
struct Download {
    #[serde(with = "hex")]
    sha1: Vec<u8>,

    size: u64,
    url: String,
}
