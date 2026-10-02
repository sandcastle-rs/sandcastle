//! Anonymous pulls from OCI registries into the blob store. This is the only
//! async code in sandcastle; it runs on a private current-thread Tokio runtime.

use std::collections::HashSet;
use std::io;
use std::pin::Pin;
use std::str::FromStr;
use std::task::{Context, Poll};

use anyhow::{Context as _, Result, bail, ensure};
use futures_util::StreamExt;
use oci_client::client::ClientConfig;
use oci_client::manifest::{ImageIndexEntry, OciDescriptor};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use oci_spec::image::{Arch, Descriptor, Digest, ImageConfiguration, MediaType, Os};
use tokio::io::AsyncWrite;

use crate::blobs::BlobStore;

/// Layer downloads in flight at once.
const PARALLEL_DOWNLOADS: usize = 4;

/// Largest image config accepted from a registry.
const MAX_CONFIG_BYTES: u64 = 16 << 20;

/// Largest layer accepted from a registry: the store disk size.
const MAX_LAYER_BYTES: u64 = 64 << 30;

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

/// The tag to name an exported image after: the reference's tag, else the
/// short digest (`sha256-<12 hex>`) of a digest reference, else `latest`.
pub fn default_tag(reference: &str) -> Result<String> {
    let parsed: Reference = reference
        .parse()
        .with_context(|| format!("invalid image reference {reference:?}"))?;
    if let Some(tag) = parsed.tag() {
        return Ok(tag.to_string());
    }
    Ok(match parsed.digest() {
        Some(digest) => {
            let (algorithm, hex) = digest.split_once(':').unwrap_or(("sha256", digest));
            format!("{algorithm}-{}", &hex[..hex.len().min(12)])
        }
        None => "latest".to_string(),
    })
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
    // oci-client buffers the manifest body itself, so its size is not bounded here.
    let (manifest, _) = client
        .pull_image_manifest(reference, &RegistryAuth::Anonymous)
        .await
        .with_context(|| format!("fetching the linux/{arch} manifest"))?;
    let config_size = u64::try_from(manifest.config.size).context("negative config size")?;
    check_size_limit(
        "config",
        &manifest.config.digest,
        config_size,
        MAX_CONFIG_BYTES,
    )?;
    let mut config_bytes = CappedWriter::new(Vec::new(), config_size);
    client
        .pull_blob(
            reference,
            &without_urls(&manifest.config),
            &mut config_bytes,
        )
        .await
        .context("downloading the image config")?;
    let config = ImageConfiguration::from_reader(&config_bytes.into_inner()[..])
        .context("parsing the image config")?;
    check_platform(&config, &arch)?;
    let layers = manifest
        .layers
        .iter()
        .map(to_descriptor)
        .collect::<Result<Vec<_>>>()?;
    for layer in &layers {
        check_size_limit(
            "layer",
            layer.digest().as_ref(),
            layer.size(),
            MAX_LAYER_BYTES,
        )?;
    }

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
    // pull_blob verifies the digest while streaming and fails on a mismatch;
    // CappedWriter stops a registry streaming past the declared size.
    // Descriptor `urls` are dropped so blobs only come from the registry.
    client
        .pull_blob(
            reference,
            &without_urls(remote),
            CappedWriter::new(file, local.size()),
        )
        .await
        .with_context(|| {
            format!(
                "downloading layer {} (declared {} bytes)",
                local.digest(),
                local.size()
            )
        })?;
    let actual = temp.as_file().metadata()?.len();
    check_downloaded_size(local.digest().as_ref(), local.size(), actual)?;
    blobs.commit(temp, local.digest())?;
    eprintln!("{}: downloaded ({} bytes)", local.digest(), local.size());
    Ok(())
}

/// Rejects a descriptor larger than `limit` before anything is downloaded.
fn check_size_limit(what: &str, digest: &str, size: u64, limit: u64) -> Result<()> {
    ensure!(
        size <= limit,
        "{what} {digest} is {size} bytes, over the {limit} byte limit"
    );
    Ok(())
}

/// Rejects a blob whose downloaded length differs from its descriptor.
fn check_downloaded_size(digest: &str, expected: u64, actual: u64) -> Result<()> {
    ensure!(
        expected == actual,
        "blob {digest} is {actual} bytes, but its descriptor says {expected}"
    );
    Ok(())
}

/// A copy of `remote` that cannot redirect the download to other hosts.
fn without_urls(remote: &OciDescriptor) -> OciDescriptor {
    OciDescriptor {
        urls: None,
        ..remote.clone()
    }
}

/// Writer that forwards at most `limit` bytes to `inner` and then fails, so a
/// registry cannot stream past the size its descriptor declared.
struct CappedWriter<W> {
    inner: W,
    remaining: u64,
    limit: u64,
}

impl<W> CappedWriter<W> {
    fn new(inner: W, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
            limit,
        }
    }

    fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for CappedWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if self.remaining == 0 {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("blob exceeds its declared size of {} bytes", self.limit),
            )));
        }
        let allowed = usize::try_from(self.remaining).map_or(buf.len(), |r| r.min(buf.len()));
        let written = match Pin::new(&mut self.inner).poll_write(cx, &buf[..allowed]) {
            Poll::Ready(Ok(n)) => n,
            other => return other,
        };
        self.remaining -= written as u64;
        Poll::Ready(Ok(written))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
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
    fn default_tag_uses_tag_or_short_digest() {
        assert_eq!(
            default_tag("mirror.gcr.io/library/busybox:1.36").unwrap(),
            "1.36"
        );
        assert_eq!(default_tag("alpine").unwrap(), "latest");
        let digest = format!("sha256:{}", "ab12".repeat(16));
        assert_eq!(
            default_tag(&format!("alpine@{digest}")).unwrap(),
            "sha256-ab12ab12ab12"
        );
    }

    #[test]
    fn oversized_descriptors_are_rejected() {
        let digest = format!("sha256:{}", "1".repeat(64));
        check_size_limit("config", &digest, MAX_CONFIG_BYTES, MAX_CONFIG_BYTES).unwrap();
        let err = check_size_limit("config", &digest, MAX_CONFIG_BYTES + 1, MAX_CONFIG_BYTES)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(&digest), "{msg}");
        assert!(msg.contains(&(MAX_CONFIG_BYTES + 1).to_string()), "{msg}");
        assert!(msg.contains(&MAX_CONFIG_BYTES.to_string()), "{msg}");
        check_size_limit("layer", &digest, MAX_LAYER_BYTES, MAX_LAYER_BYTES).unwrap();
        assert!(check_size_limit("layer", &digest, MAX_LAYER_BYTES + 1, MAX_LAYER_BYTES).is_err());
    }

    #[test]
    fn downloaded_size_must_match_descriptor() {
        let digest = format!("sha256:{}", "2".repeat(64));
        check_downloaded_size(&digest, 10, 10).unwrap();
        let err = check_downloaded_size(&digest, 10, 11).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&digest) && msg.contains("10") && msg.contains("11"),
            "{msg}"
        );
    }

    #[test]
    fn blob_urls_are_stripped_from_descriptors() {
        let mut remote = OciDescriptor {
            media_type: "application/vnd.oci.image.layer.v1.tar+gzip".into(),
            digest: format!("sha256:{}", "3".repeat(64)),
            size: 7,
            urls: Some(vec!["http://169.254.169.254/".into()]),
            annotations: Some([("k".to_string(), "v".to_string())].into()),
            ..Default::default()
        };
        let stripped = without_urls(&remote);
        assert_eq!(stripped.urls, None);
        remote.urls = None;
        assert_eq!(
            serde_json::to_value(&stripped).unwrap(),
            serde_json::to_value(&remote).unwrap()
        );
    }

    #[test]
    fn capped_writer_stops_at_the_limit() {
        use tokio::io::AsyncWriteExt;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut ok = CappedWriter::new(Vec::new(), 4);
            ok.write_all(b"ab").await.unwrap();
            ok.write_all(b"cd").await.unwrap();
            assert_eq!(ok.into_inner(), b"abcd");

            let mut over = CappedWriter::new(Vec::new(), 4);
            let err = over.write_all(b"abcde").await.unwrap_err();
            assert!(err.to_string().contains("declared size of 4"), "{err}");
            assert_eq!(over.into_inner(), b"abcd");
        });
    }
}
