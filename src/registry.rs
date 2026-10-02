//! Anonymous pulls from OCI registries into the blob store. This is the only
//! async code in sandcastle; it runs on a private current-thread Tokio runtime.

use std::collections::HashSet;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use oci_client::client::ClientConfig;
use oci_client::manifest::{ImageIndexEntry, OciDescriptor};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use oci_spec::image::{Arch, Descriptor, Digest, ImageConfiguration, MediaType, Os};

use crate::blobs::BlobStore;

/// Layer downloads in flight at once.
const PARALLEL_DOWNLOADS: usize = 4;

/// A base image whose layer blobs are all present in the blob store.
#[derive(Debug)]
pub struct PulledImage {
    pub config: ImageConfiguration,
    /// Manifest layers with OCI media types, bottom first.
    pub layers: Vec<Descriptor>,
}

/// Pulls the `linux/<host arch>` image `reference` and stores its layers.
pub fn pull(blobs: &BlobStore<'_>, reference: &str) -> Result<PulledImage> {
    let parsed: Reference = reference
        .parse()
        .with_context(|| format!("invalid image reference {reference:?}"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime
        .block_on(pull_async(blobs, &parsed))
        .with_context(|| format!("pulling {reference}"))
}

/// The tag to name an exported image after: the reference's tag, or `latest`.
pub fn default_tag(reference: &str) -> Result<String> {
    let parsed: Reference = reference
        .parse()
        .with_context(|| format!("invalid image reference {reference:?}"))?;
    Ok(parsed.tag().unwrap_or("latest").to_string())
}

async fn pull_async(blobs: &BlobStore<'_>, reference: &Reference) -> Result<PulledImage> {
    let arch = Arch::default();
    let resolver_arch = arch.clone();
    let client = Client::new(ClientConfig {
        // The default resolver matches the host OS, which is `darwin` on macOS.
        platform_resolver: Some(Box::new(move |entries: &[ImageIndexEntry]| {
            select_platform(entries, &resolver_arch)
        })),
        ..Default::default()
    });
    let (manifest, _, config_json) = client
        .pull_manifest_and_config(reference, &RegistryAuth::Anonymous)
        .await
        .with_context(|| format!("fetching the linux/{arch} manifest and config"))?;
    let config = ImageConfiguration::from_reader(config_json.as_bytes())
        .context("parsing the image config")?;
    check_platform(&config, &arch)?;
    let layers = manifest
        .layers
        .iter()
        .map(to_descriptor)
        .collect::<Result<Vec<_>>>()?;

    let mut seen = HashSet::new();
    let mut missing = Vec::new();
    for (remote, local) in manifest.layers.iter().zip(&layers) {
        if blobs.contains(local.digest())? {
            eprintln!("{}: already present", local.digest());
        } else if seen.insert(local.digest().to_string()) {
            missing.push((remote, local));
        }
    }
    let results: Vec<Result<()>> = futures_util::stream::iter(missing)
        .map(|(remote, local)| fetch_layer(&client, reference, blobs, remote, local))
        .buffer_unordered(PARALLEL_DOWNLOADS)
        .collect()
        .await;
    results.into_iter().collect::<Result<()>>()?;
    Ok(PulledImage { config, layers })
}

async fn fetch_layer(
    client: &Client,
    reference: &Reference,
    blobs: &BlobStore<'_>,
    remote: &OciDescriptor,
    local: &Descriptor,
) -> Result<()> {
    let temp = blobs.temp()?;
    let file = tokio::fs::File::from_std(temp.as_file().try_clone()?);
    // pull_blob verifies the digest while streaming and fails on a mismatch.
    client
        .pull_blob(reference, remote, file)
        .await
        .with_context(|| format!("downloading layer {}", local.digest()))?;
    blobs.commit(temp, local.digest())?;
    eprintln!("{}: downloaded ({} bytes)", local.digest(), local.size());
    Ok(())
}

/// Rejects a config for another platform; a reference that resolves straight
/// to a single manifest never goes through `select_platform`.
pub(crate) fn check_platform(config: &ImageConfiguration, arch: &Arch) -> Result<()> {
    if config.os() != &Os::Linux || config.architecture() != arch {
        bail!(
            "image is {}/{}, but linux/{arch} is required",
            config.os(),
            config.architecture()
        );
    }
    Ok(())
}

/// Picks the `linux/<arch>` manifest from an image index.
pub(crate) fn select_platform(entries: &[ImageIndexEntry], arch: &Arch) -> Option<String> {
    entries
        .iter()
        .find(|e| {
            e.platform
                .as_ref()
                .is_some_and(|p| p.os == Os::Linux && &p.architecture == arch)
        })
        .map(|e| e.digest.clone())
}

fn to_descriptor(remote: &OciDescriptor) -> Result<Descriptor> {
    let digest = Digest::from_str(&remote.digest)
        .with_context(|| format!("invalid layer digest {:?}", remote.digest))?;
    let size = u64::try_from(remote.size).context("negative layer size")?;
    Ok(Descriptor::new(
        layer_media_type(&remote.media_type)?,
        size,
        digest,
    ))
}

/// Maps registry layer media types (Docker schema2 or OCI) to OCI ones.
pub(crate) fn layer_media_type(media_type: &str) -> Result<MediaType> {
    match media_type {
        "application/vnd.oci.image.layer.v1.tar+gzip"
        | "application/vnd.docker.image.rootfs.diff.tar.gzip" => Ok(MediaType::ImageLayerGzip),
        "application/vnd.oci.image.layer.v1.tar+zstd" => Ok(MediaType::ImageLayerZstd),
        "application/vnd.oci.image.layer.v1.tar" => Ok(MediaType::ImageLayer),
        other if other.contains("foreign") || other.contains("nondistributable") => {
            bail!("non-distributable (foreign) layer {other} is not supported")
        }
        other => bail!("unsupported layer media type {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Index entries in the shape registries serve, including a BuildKit
    /// attestation manifest (`unknown/unknown`).
    fn entries() -> Vec<ImageIndexEntry> {
        serde_json::from_str(
            r#"[
            {"mediaType": "application/vnd.oci.image.manifest.v1+json", "size": 1,
             "digest": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
             "platform": {"architecture": "amd64", "os": "linux"}},
            {"mediaType": "application/vnd.oci.image.manifest.v1+json", "size": 1,
             "digest": "sha256:2222222222222222222222222222222222222222222222222222222222222222",
             "platform": {"architecture": "unknown", "os": "unknown"}},
            {"mediaType": "application/vnd.oci.image.manifest.v1+json", "size": 1,
             "digest": "sha256:3333333333333333333333333333333333333333333333333333333333333333",
             "platform": {"architecture": "arm64", "os": "darwin"}},
            {"mediaType": "application/vnd.oci.image.manifest.v1+json", "size": 1,
             "digest": "sha256:4444444444444444444444444444444444444444444444444444444444444444",
             "platform": {"architecture": "arm64", "os": "linux", "variant": "v8"}}
        ]"#,
        )
        .unwrap()
    }

    #[test]
    fn select_platform_prefers_linux_host_arch() {
        assert_eq!(
            select_platform(&entries(), &Arch::ARM64).unwrap(),
            format!("sha256:{}", "4".repeat(64))
        );
        assert_eq!(
            select_platform(&entries(), &Arch::Amd64).unwrap(),
            format!("sha256:{}", "1".repeat(64))
        );
    }

    #[test]
    fn select_platform_ignores_attestations() {
        let only_attestation: Vec<_> = entries()
            .into_iter()
            .filter(|e| e.digest.contains("2222"))
            .collect();
        assert_eq!(select_platform(&only_attestation, &Arch::Amd64), None);
        assert_eq!(select_platform(&entries(), &Arch::RISCV64), None);
    }

    #[test]
    fn layer_media_types_map_docker_to_oci() {
        assert_eq!(
            layer_media_type("application/vnd.docker.image.rootfs.diff.tar.gzip").unwrap(),
            MediaType::ImageLayerGzip
        );
        assert_eq!(
            layer_media_type("application/vnd.oci.image.layer.v1.tar+gzip").unwrap(),
            MediaType::ImageLayerGzip
        );
        assert_eq!(
            layer_media_type("application/vnd.oci.image.layer.v1.tar+zstd").unwrap(),
            MediaType::ImageLayerZstd
        );
        assert_eq!(
            layer_media_type("application/vnd.oci.image.layer.v1.tar").unwrap(),
            MediaType::ImageLayer
        );
    }

    #[test]
    fn foreign_layers_are_rejected() {
        let err = layer_media_type("application/vnd.docker.image.rootfs.foreign.diff.tar.gzip")
            .unwrap_err();
        assert!(format!("{err:#}").contains("non-distributable"), "{err:#}");
        let err = layer_media_type("application/x-unknown").unwrap_err();
        assert!(
            format!("{err:#}").contains("application/x-unknown"),
            "{err:#}"
        );
    }

    #[test]
    fn single_manifest_for_other_platform_is_rejected() {
        let config = |os: &str, arch: &str| {
            ImageConfiguration::from_reader(
                format!(r#"{{"os": "{os}", "architecture": "{arch}", "rootfs": {{"type": "layers", "diff_ids": []}}}}"#)
                    .as_bytes(),
            )
            .unwrap()
        };
        check_platform(&config("linux", "arm64"), &Arch::ARM64).unwrap();
        let err = check_platform(&config("linux", "amd64"), &Arch::ARM64).unwrap_err();
        assert!(format!("{err:#}").contains("linux/amd64"), "{err:#}");
        let err = check_platform(&config("windows", "arm64"), &Arch::ARM64).unwrap_err();
        assert!(format!("{err:#}").contains("windows/arm64"), "{err:#}");
    }

    #[test]
    fn default_tag_falls_back_to_latest() {
        assert_eq!(
            default_tag("mirror.gcr.io/library/busybox:1.36").unwrap(),
            "1.36"
        );
        assert_eq!(default_tag("alpine").unwrap(), "latest");
    }
}
