//! OCI image assembly: the mutable configuration state of an image being
//! built, gzip layer blobs, and the OCI image layout written at the end.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, bail, ensure};
use flate2::Compression;
use flate2::write::GzEncoder;
use oci_spec::image::{
    Arch, ConfigBuilder, Descriptor, Digest, History, HistoryBuilder, ImageConfiguration,
    ImageConfigurationBuilder, ImageIndexBuilder, ImageManifestBuilder, MediaType, Os,
    RootFsBuilder,
};
use sandcastle_proto::{LAYER_TAR, LAYER_TAR_GZIP, LowerLayer};
use serde::Serialize;
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

    /// The layer stack as guest jobs take it, bottom first. Only layer
    /// formats the guest can unpack are accepted.
    pub fn lower_layers(&self) -> Result<Vec<LowerLayer>> {
        self.layers
            .iter()
            .zip(&self.diff_ids)
            .map(|(desc, diff_id)| {
                let media_type = match desc.media_type() {
                    MediaType::ImageLayer => LAYER_TAR,
                    MediaType::ImageLayerGzip => LAYER_TAR_GZIP,
                    other => bail!(
                        "layer {} is {other}; sandcastle can only unpack uncompressed and gzip layers (zstd is not supported yet)",
                        desc.digest()
                    ),
                };
                Ok(LowerLayer {
                    diff_id: diff_id.to_string(),
                    blob: desc.digest().to_string(),
                    media_type: media_type.to_string(),
                })
            })
            .collect()
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
        (
            self.inner,
            digest_from(&self.hasher.finalize().into()),
            self.len,
        )
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

/// Annotation tools such as skopeo and podman use to find an image in a layout.
pub const REF_NAME_ANNOTATION: &str = "org.opencontainers.image.ref.name";
const SCHEMA_VERSION: u32 = 2;
const OCI_LAYOUT: &[u8] = br#"{"imageLayoutVersion":"1.0.0"}"#;

/// Serializes with sorted object keys, so equal values give equal digests.
pub fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&serde_json::to_value(value)?)?)
}

/// Writes `state` as the single image of an OCI layout at `out_dir`, named
/// `ref_name`. An existing layout is updated in place: blobs are content
/// addressed, and `index.json` is replaced to point at this image only.
pub fn write_layout(
    blobs: &BlobStore<'_>,
    state: &ConfigState,
    out_dir: &Path,
    ref_name: &str,
) -> Result<Descriptor> {
    let config = blobs.put_bytes(
        MediaType::ImageConfig,
        &canonical_json(&state.to_configuration()?)?,
    )?;
    let manifest = ImageManifestBuilder::default()
        .schema_version(SCHEMA_VERSION)
        .media_type(MediaType::ImageManifest)
        .config(config.clone())
        .layers(state.layers.clone())
        .build()?;
    let mut manifest_desc =
        blobs.put_bytes(MediaType::ImageManifest, &canonical_json(&manifest)?)?;

    let out_blobs = out_dir.join("blobs").join("sha256");
    fs::create_dir_all(&out_blobs).with_context(|| format!("creating {}", out_blobs.display()))?;
    for desc in state.layers.iter().chain([&config, &manifest_desc]) {
        let dest = out_blobs.join(desc.digest().digest());
        let present =
            fs::symlink_metadata(&dest).is_ok_and(|m| m.is_file() && m.len() == desc.size());
        if !present {
            // A unique temp file renamed over `dest` replaces a planted
            // symlink instead of writing through it.
            let mut temp = NamedTempFile::new_in(&out_blobs)
                .with_context(|| format!("creating a temporary file in {}", out_blobs.display()))?;
            let mut src = File::open(blobs.path(desc.digest())?)?;
            io::copy(&mut src, &mut temp)
                .with_context(|| format!("copying blob {} into the layout", desc.digest()))?;
            temp.persist(&dest)
                .map_err(|e| e.error)
                .with_context(|| format!("storing blob {} in the layout", desc.digest()))?;
        }
    }

    write_atomic(&out_dir.join("oci-layout"), OCI_LAYOUT)?;
    manifest_desc.set_annotations(Some(HashMap::from([(
        REF_NAME_ANNOTATION.to_string(),
        ref_name.to_string(),
    )])));
    let index = ImageIndexBuilder::default()
        .schema_version(SCHEMA_VERSION)
        .media_type(MediaType::ImageIndex)
        .manifests(vec![manifest_desc.clone()])
        .build()?;
    write_atomic(&out_dir.join("index.json"), &canonical_json(&index)?)?;
    Ok(manifest_desc)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .context("layout file has no parent directory")?;
    let mut temp = NamedTempFile::new_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    temp.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    temp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Write};
    use std::path::Path;

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
    fn lower_layers_map_media_types_and_reject_zstd() {
        let d =
            |n: u8| Digest::from_str(&format!("sha256:{}", format!("{n:02x}").repeat(32))).unwrap();
        let mut state = ConfigState::from_base(&docker_config(), vec![base_layer()]).unwrap();
        state.layers = vec![
            Descriptor::new(MediaType::ImageLayerGzip, 1, d(1)),
            Descriptor::new(MediaType::ImageLayer, 1, d(2)),
        ];
        state.diff_ids = vec![d(3), d(4)];
        let lower = state.lower_layers().unwrap();
        assert_eq!(lower[0].media_type, sandcastle_proto::LAYER_TAR_GZIP);
        assert_eq!(lower[0].blob, d(1).to_string());
        assert_eq!(lower[1].diff_id, d(4).to_string());
        assert_eq!(lower[1].media_type, sandcastle_proto::LAYER_TAR);

        state.layers[1] = Descriptor::new(MediaType::ImageLayerZstd, 1, d(2));
        let err = state.lower_layers().unwrap_err();
        assert!(format!("{err:#}").contains("zstd"), "{err:#}");
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

    use oci_spec::image::{ImageIndex, ImageManifest};

    fn built_state(blobs: &BlobStore<'_>, cmd: &str) -> ConfigState {
        let mut base = docker_config();
        base.rootfs_mut().set_diff_ids(vec![]);
        let mut state = ConfigState::from_base(&base, vec![]).unwrap();
        let mut writer = LayerWriter::new(blobs).unwrap();
        writer.write_all(b"layer contents").unwrap();
        state
            .add_layer(writer.finish().unwrap(), "RUN make")
            .unwrap();
        state.cmd = Some(vec![cmd.to_string()]);
        state
    }

    /// Re-hashes a blob in the layout and checks it against its descriptor.
    fn read_verified(out: &Path, desc: &Descriptor) -> Vec<u8> {
        let bytes = fs::read(out.join("blobs/sha256").join(desc.digest().digest())).unwrap();
        assert_eq!(&sha256(&bytes), desc.digest());
        assert_eq!(bytes.len() as u64, desc.size());
        bytes
    }

    #[test]
    fn write_layout_produces_verifiable_layout() {
        let (dir, store) = test_support::store();
        let blobs = store.blobs();
        let state = built_state(&blobs, "app");
        let out = dir.path().join("out layout");
        write_layout(&blobs, &state, &out, "demo").unwrap();

        let layout: serde_json::Value =
            serde_json::from_slice(&fs::read(out.join("oci-layout")).unwrap()).unwrap();
        assert_eq!(layout["imageLayoutVersion"], "1.0.0");
        let index = ImageIndex::from_file(out.join("index.json")).unwrap();
        assert_eq!(index.manifests().len(), 1);
        let entry = &index.manifests()[0];
        assert_eq!(entry.media_type(), &MediaType::ImageManifest);
        assert_eq!(
            entry.annotations().as_ref().unwrap()[REF_NAME_ANNOTATION],
            "demo"
        );

        let manifest = ImageManifest::from_reader(&read_verified(&out, entry)[..]).unwrap();
        assert_eq!(manifest.config().media_type(), &MediaType::ImageConfig);
        let config =
            ImageConfiguration::from_reader(&read_verified(&out, manifest.config())[..]).unwrap();
        assert_eq!(
            config.rootfs().diff_ids(),
            &vec![state.diff_ids[0].to_string()]
        );
        assert_eq!(
            config.config().as_ref().unwrap().cmd(),
            &Some(vec!["app".to_string()])
        );
        assert_eq!(manifest.layers().len(), 1);
        read_verified(&out, &manifest.layers()[0]);
    }

    #[cfg(unix)]
    #[test]
    fn write_layout_does_not_follow_planted_symlinks() {
        use std::os::unix::fs::symlink;
        let (dir, store) = test_support::store();
        let blobs = store.blobs();
        let state = built_state(&blobs, "app");
        let out = dir.path().join("out");
        fs::create_dir_all(out.join("blobs/sha256")).unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, "precious").unwrap();
        let layer_hex = state.layers[0].digest().digest().to_string();
        symlink(&victim, out.join("blobs/sha256").join(&layer_hex)).unwrap();
        symlink(
            &victim,
            out.join("blobs/sha256")
                .join(format!("{layer_hex}.partial")),
        )
        .unwrap();
        for name in [
            "index.json.tmp",
            "oci-layout.tmp",
            "index.tmp",
            "oci-layout",
        ] {
            symlink(&victim, out.join(name)).unwrap();
        }

        let manifest = write_layout(&blobs, &state, &out, "demo").unwrap();

        assert_eq!(fs::read_to_string(&victim).unwrap(), "precious");
        let index = ImageIndex::from_file(out.join("index.json")).unwrap();
        assert_eq!(index.manifests()[0].digest(), manifest.digest());
        let layer_path = out.join("blobs/sha256").join(&layer_hex);
        assert!(fs::symlink_metadata(&layer_path).unwrap().is_file());
        read_verified(&out, &state.layers[0]);
    }

    #[test]
    fn write_layout_replaces_wrong_size_blob() {
        let (dir, store) = test_support::store();
        let blobs = store.blobs();
        let state = built_state(&blobs, "app");
        let out = dir.path().join("out");
        fs::create_dir_all(out.join("blobs/sha256")).unwrap();
        let layer_hex = state.layers[0].digest().digest().to_string();
        fs::write(out.join("blobs/sha256").join(&layer_hex), "truncat").unwrap();
        write_layout(&blobs, &state, &out, "demo").unwrap();
        read_verified(&out, &state.layers[0]);
    }

    #[test]
    fn rewriting_layout_points_index_at_latest_image() {
        let (dir, store) = test_support::store();
        let blobs = store.blobs();
        let out = dir.path().join("out");
        let first = write_layout(&blobs, &built_state(&blobs, "one"), &out, "demo").unwrap();
        let second = write_layout(&blobs, &built_state(&blobs, "two"), &out, "demo").unwrap();
        assert_ne!(first.digest(), second.digest());
        let index = ImageIndex::from_file(out.join("index.json")).unwrap();
        assert_eq!(index.manifests().len(), 1);
        assert_eq!(index.manifests()[0].digest(), second.digest());
        read_verified(&out, &index.manifests()[0]);
    }

    #[test]
    fn config_json_is_identical_for_identical_state() {
        let (_dir, store) = test_support::store();
        let blobs = store.blobs();
        let mut state = built_state(&blobs, "app");
        for i in 0..32 {
            state.labels.insert(format!("label.{i}"), i.to_string());
        }
        // Each conversion builds a new HashMap with its own random iteration order.
        let a = canonical_json(&state.to_configuration().unwrap()).unwrap();
        let b = canonical_json(&state.to_configuration().unwrap()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn canonical_json_sorts_keys_at_every_level() {
        let owned: Vec<String> = (0..16).map(|i| format!("k{i:02}")).collect();
        let keys: Vec<&str> = owned.iter().map(String::as_str).collect();
        fn outer<'a>(order: &[&'a str]) -> HashMap<&'static str, HashMap<&'a str, &'static str>> {
            let inner: HashMap<&str, &str> = order.iter().map(|k| (*k, "v")).collect();
            HashMap::from([("z", inner.clone()), ("a", inner)])
        }
        let forward = canonical_json(&outer(&keys)).unwrap();
        let reversed: Vec<&str> = keys.iter().rev().copied().collect();
        let backward = canonical_json(&outer(&reversed)).unwrap();
        assert_eq!(forward, backward);
        let expected_inner = format!(
            "{{{}}}",
            keys.iter()
                .map(|k| format!("\"{k}\":\"v\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert_eq!(
            String::from_utf8(forward).unwrap(),
            format!("{{\"a\":{expected_inner},\"z\":{expected_inner}}}")
        );
    }
}
