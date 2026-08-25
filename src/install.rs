//! Checksum-verified installation of the pinned connector driver.

use std::{
    fs,
    io::{Cursor, Read},
    path::Path,
};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

use crate::{WACLI_VERSION, config::OrbitPaths};

const RELEASE_BASE: &str = "https://github.com/openclaw/wacli/releases/download/v0.15.0";

pub async fn install_wacli(paths: &OrbitPaths, force: bool) -> Result<()> {
    paths.create()?;
    let destination = paths.wacli_binary();
    if destination.exists() && !force {
        return Ok(());
    }
    let (asset, expected) = release_asset()?;
    let url = format!("{RELEASE_BASE}/{asset}");
    let bytes = reqwest::get(&url)
        .await
        .context("download pinned wacli")?
        .error_for_status()?
        .bytes()
        .await?;
    let actual = hex::encode(Sha256::digest(&bytes));
    if actual != expected {
        bail!("wacli checksum mismatch: expected {expected}, received {actual}");
    }
    extract_binary(asset, &bytes, &destination)?;
    #[cfg(unix)]
    set_executable(&destination)?;
    Ok(())
}

fn release_asset() -> Result<(&'static str, &'static str)> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Ok((
            "wacli_0.15.0_windows_amd64.zip",
            "b88ba64f6931cbb46b297d15e3497abb44396d7ded9f4956685117ee2a7e406a",
        )),
        ("linux", "x86_64") => Ok((
            "wacli_0.15.0_linux_amd64.tar.gz",
            "e01903177e3e4c93dc962e70dcc07bc3dee3a4b282a5e3096f8a5228516e7bb7",
        )),
        ("linux", "aarch64") => Ok((
            "wacli_0.15.0_linux_arm64.tar.gz",
            "6f6e523b6c14f7e413af436f8066a1370573d29a858be06670c12350fa27c7ec",
        )),
        ("macos", "x86_64") => Ok((
            "wacli_0.15.0_darwin_amd64.tar.gz",
            "f778377aa1ef317335284166d5832b492e2a99aae107ae475aaf54c0940ce1a7",
        )),
        ("macos", "aarch64") => Ok((
            "wacli_0.15.0_darwin_arm64.tar.gz",
            "2b54f33d246e913a5c33525b4fc895a345363c2dcc673c70fa5f19cffb15d17d",
        )),
        (os, arch) => bail!("wacli {WACLI_VERSION} has no pinned build for {os}/{arch}"),
    }
}

fn extract_binary(asset: &str, bytes: &[u8], destination: &Path) -> Result<()> {
    let mut output = Vec::new();
    if Path::new(asset)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
    {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
        let index = (0..archive.len())
            .find(|&i| {
                archive
                    .by_index(i)
                    .is_ok_and(|f| f.name().ends_with("wacli.exe"))
            })
            .context("release archive lacks wacli.exe")?;
        archive.by_index(index)?.read_to_end(&mut output)?;
    } else {
        let mut archive = tar::Archive::new(GzDecoder::new(bytes));
        let mut found = false;
        for entry in archive.entries()? {
            let mut entry = entry?;
            if entry
                .path()?
                .file_name()
                .is_some_and(|name| name == "wacli")
            {
                entry.read_to_end(&mut output)?;
                found = true;
                break;
            }
        }
        if !found {
            bail!("release archive lacks wacli");
        }
    }
    let temporary = destination.with_extension("download");
    fs::write(&temporary, output)?;
    replace_binary(&temporary, destination)?;
    Ok(())
}

fn replace_binary(temporary: &Path, destination: &Path) -> Result<()> {
    // Windows rename does not replace an existing file. The new artifact has
    // already passed checksum and archive validation before the old pinned
    // driver is removed, keeping --force-driver failure-safe until this point.
    if destination.exists() {
        fs::remove_file(destination)
            .with_context(|| format!("replace {}", destination.display()))?;
    }
    fs::rename(temporary, destination).with_context(|| format!("install {}", destination.display()))
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn supported_platform_has_a_sha256_checksum() {
        let (asset, digest) = release_asset().unwrap();
        assert!(asset.contains(WACLI_VERSION));
        assert_eq!(digest.len(), 64);
    }

    #[test]
    fn prepared_driver_replaces_an_existing_binary() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("wacli");
        let prepared = temp.path().join("wacli.download");
        fs::write(&destination, b"old").unwrap();
        fs::write(&prepared, b"verified-new").unwrap();
        replace_binary(&prepared, &destination).unwrap();
        assert_eq!(fs::read(destination).unwrap(), b"verified-new");
        assert!(!prepared.exists());
    }
}
