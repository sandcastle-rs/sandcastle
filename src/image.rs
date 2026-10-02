//! OCI image assembly: the mutable configuration state of an image being
//! built, gzip layer blobs, and the OCI image layout written at the end.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::str::FromStr;

use anyhow::{Context, Result, ensure};
use flate2::Compression;
use flate2::write::GzEncoder;
use oci_spec::image::{
    Arch, ConfigBuilder, Descriptor, Digest, History, HistoryBuilder, ImageConfiguration,
    ImageConfigurationBuilder, MediaType, Os, RootFsBuilder,
};
use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;

use crate::blobs::{BlobStore, digest_from};

/// Output buffer for compressed layer bytes.
const LAYER_BUFFER: usize = 1 << 20;

/// A layer blob in the store and the digest of its uncompressed tar.
#[derive(Debug, Clone)]
pub struct Layer {
    pub descriptor: Descriptor,
    pub diff_id: Digest,
}

/// Everything sandcastle tracks about an image while building it.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigState {
    pub architecture: Arch,
    pub os: Os,
    pub variant: Option<String>,
    pub os_version: Option<String>,
    pub env: Vec<String>,
    pub cmd: Option<Vec<String>>,
    pub entrypoint: Option<Vec<String>>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub exposed_ports: BTreeSet<String>,
    pub volumes: BTreeSet<String>,
    pub stop_signal: Option<String>,
    pub history: Vec<History>,
    /// Layer blobs, bottom first; `diff_ids[i]` belongs to `layers[i]`.
    pub layers: Vec<Descriptor>,
    pub diff_ids: Vec<Digest>,
}

impl ConfigState {
    /// Starts from a base image's config and its manifest layers.
    pub fn from_base(config: &ImageConfiguration, layers: Vec<Descriptor>) -> Result<Self> {
        let diff_ids = config
            .rootfs()
            .diff_ids()
            .iter()
            .map(|d| Digest::from_str(d))
            .collect::<Result<Vec<_>, _>>()
            .context("base image config has an invalid diff_id")?;
        ensure!(
            diff_ids.len() == layers.len(),
            "base image is inconsistent: {} layers in the manifest but {} diff_ids in the config",
            layers.len(),
            diff_ids.len()
        );
        let c = config.config().clone().unwrap_or_default();
        Ok(Self {
            architecture: config.architecture().clone(),
            os: config.os().clone(),
            variant: config.variant().clone(),
            os_version: config.os_version().clone(),
            env: c.env().clone().unwrap_or_default(),
            cmd: c.cmd().clone(),
            entrypoint: c.entrypoint().clone(),
            working_dir: c.working_dir().clone(),
            user: c.user().clone(),
            labels: c.labels().clone().unwrap_or_default().into_iter().collect(),
            exposed_ports: c
                .exposed_ports()
                .clone()
                .unwrap_or_default()
                .into_iter()
                .collect(),
            volumes: c
                .volumes()
                .clone()
                .unwrap_or_default()
                .into_iter()
                .collect(),
            stop_signal: c.stop_signal().clone(),
            history: config.history().clone().unwrap_or_default(),
            layers,
            diff_ids,
        })
    }

    pub fn add_layer(&mut self, layer: Layer, created_by: &str) -> Result<()> {
        self.layers.push(layer.descriptor);
        self.diff_ids.push(layer.diff_id);
        self.history
            .push(HistoryBuilder::default().created_by(created_by).build()?);
        Ok(())
    }

    pub fn add_empty(&mut self, created_by: &str) -> Result<()> {
        self.history.push(
            HistoryBuilder::default()
                .created_by(created_by)
                .empty_layer(true)
                .build()?,
        );
        Ok(())
    }

    pub fn to_configuration(&self) -> Result<ImageConfiguration> {
        let mut config = ConfigBuilder::default();
        if !self.env.is_empty() {
            config = config.env(self.env.clone());
        }
        if let Some(cmd) = &self.cmd {
            config = config.cmd(cmd.clone());
        }
        if let Some(entrypoint) = &self.entrypoint {
            config = config.entrypoint(entrypoint.clone());
        }
        if let Some(dir) = &self.working_dir {
            config = config.working_dir(dir.clone());
        }
        if let Some(user) = &self.user {
            config = config.user(user.clone());
        }
        if !self.labels.is_empty() {
            config = config.labels(self.labels.clone().into_iter().collect::<HashMap<_, _>>());
        }
        if !self.exposed_ports.is_empty() {
            config = config.exposed_ports(self.exposed_ports.iter().cloned().collect::<Vec<_>>());
        }
        if !self.volumes.is_empty() {
            config = config.volumes(self.volumes.iter().cloned().collect::<Vec<_>>());
        }
        if let Some(signal) = &self.stop_signal {
            config = config.stop_signal(signal.clone());
        }
        let rootfs = RootFsBuilder::default()
            .typ("layers")
            .diff_ids(
                self.diff_ids
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
            )
            .build()?;
        let mut image = ImageConfigurationBuilder::default()
            .architecture(self.architecture.clone())
            .os(self.os.clone())
            .config(config.build()?)
            .rootfs(rootfs)
            .history(self.history.clone());
        if let Some(variant) = &self.variant {
            image = image.variant(variant.clone());
        }
        if let Some(version) = &self.os_version {
            image = image.os_version(version.clone());
        }
        Ok(image.build()?)
    }
}

/// Passes bytes through to `inner` while hashing them.
struct Hashing<W> {
    inner: W,
    hasher: Sha256,
    len: u64,
}

impl<W> Hashing<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            len: 0,
        }
    }

    fn into_parts(self) -> (W, Digest, u64) {
        (self.inner, digest_from(&self.hasher.finalize()), self.len)
    }
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Streams an uncompressed layer tar into a gzip blob. Each byte is hashed
/// once on the way in (diff_id) and once on the way out (blob digest).
pub struct LayerWriter<'b, 's> {
    blobs: &'b BlobStore<'s>,
    temp: NamedTempFile,
    sink: Hashing<GzEncoder<Hashing<BufWriter<File>>>>,
}

impl<'b, 's> LayerWriter<'b, 's> {
    pub fn new(blobs: &'b BlobStore<'s>) -> Result<Self> {
        let temp = blobs.temp()?;
        let file = BufWriter::with_capacity(LAYER_BUFFER, temp.as_file().try_clone()?);
        let sink = Hashing::new(GzEncoder::new(Hashing::new(file), Compression::default()));
        Ok(Self { blobs, temp, sink })
    }

    pub fn finish(self) -> Result<Layer> {
        let Self { blobs, temp, sink } = self;
        let (encoder, diff_id, _) = sink.into_parts();
        let (file, digest, size) = encoder.finish()?.into_parts();
        file.into_inner().map_err(io::IntoInnerError::into_error)?;
        blobs.commit(temp, &digest)?;
        Ok(Layer {
            descriptor: Descriptor::new(MediaType::ImageLayerGzip, size, digest),
            diff_id,
        })
    }
}

impl Write for LayerWriter<'_, '_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.sink.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sink.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Write};

    use super::*;
    use crate::blobs::sha256;
    use crate::store::test_support;

    /// A Docker-style config as served by registries (extra keys included).
    const DOCKER_CONFIG: &str = r#"{
        "architecture": "arm64",
        "variant": "v8",
        "os": "linux",
        "container_config": {"Hostname": "abc"},
        "config": {
            "Env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
            "Cmd": ["sh"],
            "WorkingDir": "/srv",
            "User": "1000:1000",
            "Labels": {"org.example.b": "2", "org.example.a": "1"},
            "ExposedPorts": {"8080/tcp": {}},
            "StopSignal": "SIGQUIT"
        },
        "rootfs": {"type": "layers", "diff_ids": [
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ]},
        "history": [
            {"created_by": "ADD rootfs.tar /"},
            {"created_by": "CMD [\"sh\"]", "empty_layer": true}
        ]
    }"#;

    fn base_layer() -> Descriptor {
        Descriptor::new(
            MediaType::ImageLayerGzip,
            10,
            Digest::from_str(
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            )
            .unwrap(),
        )
    }

    fn docker_config() -> ImageConfiguration {
        ImageConfiguration::from_reader(DOCKER_CONFIG.as_bytes()).unwrap()
    }

    #[test]
    fn from_base_reads_docker_config() {
        let state = ConfigState::from_base(&docker_config(), vec![base_layer()]).unwrap();
        assert_eq!(state.architecture, Arch::ARM64);
        assert_eq!(state.variant.as_deref(), Some("v8"));
        assert_eq!(state.cmd, Some(vec!["sh".to_string()]));
        assert_eq!(state.working_dir.as_deref(), Some("/srv"));
        assert_eq!(state.user.as_deref(), Some("1000:1000"));
        assert_eq!(
            state.labels.keys().collect::<Vec<_>>(),
            ["org.example.a", "org.example.b"]
        );
        assert!(state.exposed_ports.contains("8080/tcp"));
        assert_eq!(state.stop_signal.as_deref(), Some("SIGQUIT"));
        assert_eq!(state.history.len(), 2);
        assert_eq!(state.diff_ids.len(), 1);
    }

    #[test]
    fn from_base_rejects_layer_count_mismatch() {
        let err =
            ConfigState::from_base(&docker_config(), vec![base_layer(), base_layer()]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("2 layers") && msg.contains("1 diff_ids"),
            "{msg}"
        );
    }

    #[test]
    fn to_configuration_keeps_every_field() {
        let state = ConfigState::from_base(&docker_config(), vec![base_layer()]).unwrap();
        let again =
            ConfigState::from_base(&state.to_configuration().unwrap(), vec![base_layer()]).unwrap();
        assert_eq!(again, state);
    }

    #[test]
    fn layer_writer_hashes_tar_and_gzip_in_one_pass() {
        let (_dir, store) = test_support::store();
        let blobs = store.blobs();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut writer = LayerWriter::new(&blobs).unwrap();
        for chunk in data.chunks(4096) {
            writer.write_all(chunk).unwrap();
        }
        let layer = writer.finish().unwrap();

        let blob = fs::read(blobs.path(layer.descriptor.digest()).unwrap()).unwrap();
        assert_eq!(layer.descriptor.size(), blob.len() as u64);
        assert_eq!(layer.descriptor.digest(), &sha256(&blob));
        assert_eq!(layer.descriptor.media_type(), &MediaType::ImageLayerGzip);
        assert_eq!(layer.diff_id, sha256(&data));
        let mut unpacked = Vec::new();
        flate2::read::GzDecoder::new(&blob[..])
            .read_to_end(&mut unpacked)
            .unwrap();
        assert_eq!(unpacked, data);
    }

    #[test]
    fn add_layer_and_add_empty_record_history() {
        let (_dir, store) = test_support::store();
        let blobs = store.blobs();
        let mut state = ConfigState::from_base(&docker_config(), vec![base_layer()]).unwrap();
        let mut writer = LayerWriter::new(&blobs).unwrap();
        writer.write_all(b"tar bytes").unwrap();
        let layer = writer.finish().unwrap();
        let diff_id = layer.diff_id.clone();
        state.add_layer(layer, "RUN true").unwrap();
        state.add_empty("ENV A=b").unwrap();
        assert_eq!(state.layers.len(), 2);
        assert_eq!(state.diff_ids.last(), Some(&diff_id));
        let last_two: Vec<_> = state.history[2..]
            .iter()
            .map(|h| (h.created_by().clone(), h.empty_layer()))
            .collect();
        assert_eq!(
            last_two,
            [
                (Some("RUN true".to_string()), None),
                (Some("ENV A=b".to_string()), Some(true))
            ]
        );
    }
}
