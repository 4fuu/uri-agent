use anyhow::{Context, Result, anyhow, bail};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Mutex;
use uuid::Uuid;

const MAX_DOWNLOAD_BYTES: usize = 100 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug)]
pub enum DownloadArchive {
    TarGz,
    Zip,
}

#[derive(Clone, Debug)]
pub struct BinaryDownload {
    pub name: &'static str,
    pub version: &'static str,
    pub url: String,
    pub sha256: &'static str,
    pub archive: DownloadArchive,
    pub archive_path: &'static str,
    pub executable_name: &'static str,
}

#[derive(Clone)]
pub(crate) struct BinaryDownloader {
    directory: PathBuf,
    client: reqwest::Client,
    process_lock: Arc<Mutex<()>>,
}

impl Default for BinaryDownloader {
    fn default() -> Self {
        Self {
            directory: dirs::cache_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("uri-agent")
                .join("tools"),
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(DOWNLOAD_TIMEOUT)
                .build()
                .expect("binary downloader HTTP client configuration is valid"),
            process_lock: Arc::new(Mutex::new(())),
        }
    }
}

impl BinaryDownloader {
    pub async fn ensure(&self, spec: &BinaryDownload) -> Result<PathBuf> {
        if let Some(path) = executable_on_path(spec.executable_name).await {
            return Ok(path);
        }
        let destination = self
            .directory
            .join(spec.name)
            .join(spec.version)
            .join(platform_executable_name(spec.executable_name));
        if validate_executable(&destination, spec).await {
            return Ok(destination);
        }

        let _process_guard = self.process_lock.lock().await;
        tokio::fs::create_dir_all(destination.parent().expect("tool path has a parent"))
            .await
            .with_context(|| format!("cannot create tool cache for {}", spec.name))?;
        let lock_path = destination
            .parent()
            .expect("tool path has a parent")
            .join("install.lock");
        let lock = tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&lock_path)?;
            file.lock_exclusive()?;
            Ok(file)
        })
        .await
        .context("binary installation lock worker failed")??;

        if validate_executable(&destination, spec).await {
            drop(lock);
            return Ok(destination);
        }
        let _ = tokio::fs::remove_file(&destination).await;
        let mut response = self
            .client
            .get(&spec.url)
            .send()
            .await
            .with_context(|| format!("failed to download {} {}", spec.name, spec.version))?
            .error_for_status()
            .with_context(|| format!("failed to download {} {}", spec.name, spec.version))?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_DOWNLOAD_BYTES as u64)
        {
            bail!(
                "download for {} exceeds {MAX_DOWNLOAD_BYTES} bytes",
                spec.name
            );
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len().saturating_add(chunk.len()) > MAX_DOWNLOAD_BYTES {
                bail!(
                    "download for {} exceeds {MAX_DOWNLOAD_BYTES} bytes",
                    spec.name
                );
            }
            bytes.extend_from_slice(&chunk);
        }
        let digest = format!("{:x}", Sha256::digest(&bytes));
        if !digest.eq_ignore_ascii_case(spec.sha256) {
            bail!(
                "checksum mismatch for {} {}: expected {}, got {}",
                spec.name,
                spec.version,
                spec.sha256,
                digest
            );
        }

        let archive = spec.archive;
        let archive_path = spec.archive_path.to_string();
        let executable =
            tokio::task::spawn_blocking(move || extract_executable(&bytes, archive, &archive_path))
                .await
                .context("binary extraction worker failed")??;
        let temporary = destination.with_file_name(format!(
            ".{}.{}.tmp",
            spec.executable_name,
            Uuid::now_v7().simple()
        ));
        if let Err(error) = tokio::fs::write(&temporary, executable).await {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(error).with_context(|| format!("cannot write downloaded {}", spec.name));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(error) =
                tokio::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o755)).await
            {
                let _ = tokio::fs::remove_file(&temporary).await;
                return Err(error)
                    .with_context(|| format!("cannot make downloaded {} executable", spec.name));
            }
        }
        if !validate_executable(&temporary, spec).await {
            let _ = tokio::fs::remove_file(&temporary).await;
            bail!(
                "downloaded {} {} failed version validation",
                spec.name,
                spec.version
            );
        }
        if let Err(error) = tokio::fs::rename(&temporary, &destination).await {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(error).with_context(|| format!("cannot install {}", spec.name));
        }
        drop(lock);
        Ok(destination)
    }
}

async fn executable_on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&paths) {
        let candidate = directory.join(platform_executable_name(name));
        if version_output(&candidate)
            .await
            .is_some_and(|output| output.status.success())
        {
            return Some(candidate);
        }
    }
    None
}

fn platform_executable_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

async fn validate_executable(path: &Path, spec: &BinaryDownload) -> bool {
    let Some(output) = version_output(path).await else {
        return false;
    };
    output.status.success()
        && String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .is_some_and(|line| line.contains(spec.version))
}

async fn version_output(path: &Path) -> Option<std::process::Output> {
    tokio::time::timeout(
        VERSION_TIMEOUT,
        Command::new(path)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()
}

fn extract_executable(
    bytes: &[u8],
    archive: DownloadArchive,
    archive_path: &str,
) -> Result<Vec<u8>> {
    match archive {
        DownloadArchive::TarGz => {
            let decoder = flate2::read::GzDecoder::new(Cursor::new(bytes));
            let mut archive = tar::Archive::new(decoder);
            for entry in archive.entries()? {
                let mut entry = entry?;
                if entry.path()?.as_ref() == Path::new(archive_path) {
                    let mut executable = Vec::new();
                    entry.read_to_end(&mut executable)?;
                    return Ok(executable);
                }
            }
        }
        DownloadArchive::Zip => {
            let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
            let mut entry = archive
                .by_name(archive_path)
                .with_context(|| format!("archive does not contain {archive_path}"))?;
            let mut executable = Vec::new();
            entry.read_to_end(&mut executable)?;
            return Ok(executable);
        }
    }
    Err(anyhow!("archive does not contain {archive_path}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;
    use zip::{ZipWriter, write::SimpleFileOptions};

    #[test]
    fn extracts_only_the_requested_tar_gz_member() {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let content = b"ripgrep executable";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(&mut header, "ripgrep/rg", content.as_slice())
            .unwrap();
        let bytes = archive.into_inner().unwrap().finish().unwrap();

        assert_eq!(
            extract_executable(&bytes, DownloadArchive::TarGz, "ripgrep/rg").unwrap(),
            content
        );
        assert!(extract_executable(&bytes, DownloadArchive::TarGz, "other/rg").is_err());
    }

    #[test]
    fn extracts_only_the_requested_zip_member() {
        let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
        archive
            .start_file("ripgrep/rg.exe", SimpleFileOptions::default())
            .unwrap();
        archive.write_all(b"ripgrep executable").unwrap();
        let bytes = archive.finish().unwrap().into_inner();

        assert_eq!(
            extract_executable(&bytes, DownloadArchive::Zip, "ripgrep/rg.exe").unwrap(),
            b"ripgrep executable"
        );
        assert!(extract_executable(&bytes, DownloadArchive::Zip, "other/rg.exe").is_err());
    }
}
