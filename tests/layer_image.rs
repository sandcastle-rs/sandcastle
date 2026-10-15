//! Layer image tests: need the bundled `lib/` (mkfs.erofs). Run with `just it`.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use sandcastle::blobs::sha256;
use sandcastle::install::Install;
use sandcastle::store::Store;

/// A store whose install is the signed test binary's.
fn store() -> (tempfile::TempDir, Store) {
    let exe = PathBuf::from(std::env::var_os("SANDCASTLE_BIN").expect("SANDCASTLE_BIN"));
    let install = Install::locate(&exe).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("store"), &install).unwrap();
    (dir, store)
}

/// A tar of `entries` (path, entry type, mode, uid), files holding `data`.
fn tar(entries: &[(&str, tar::EntryType, u32, u64)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for &(path, kind, mode, uid) in entries {
        let mut header = tar::Header::new_ustar();
        header.set_entry_type(kind);
        header.set_mode(mode);
        header.set_uid(uid);
        header.set_gid(uid);
        let data: &[u8] = if kind == tar::EntryType::Regular {
            b"data\n"
        } else {
            b""
        };
        header.set_size(data.len() as u64);
        builder.append_data(&mut header, path, data).unwrap();
    }
    builder.into_inner().unwrap()
}

/// Root directory of an erofs image: (mode, uid, gid), and its block size.
fn root_inode(image: &Path) -> ((u32, u32, u32), u32) {
    let bytes = std::fs::read(image).unwrap();
    let sb = &bytes[1024..];
    let u16_at = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32_at = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    assert_eq!(u32_at(sb, 0), 0xE0F5_E1E2, "erofs magic");
    let block = 1u32 << sb[12];
    let root_nid = u16_at(sb, 14) as usize;
    let meta = u32_at(sb, 40) as usize * block as usize;
    let inode = &bytes[meta + root_nid * 32..];
    let mode = u32::from(u16_at(inode, 4));
    let ids = if u16_at(inode, 0) & 1 == 0 {
        (u32::from(u16_at(inode, 24)), u32::from(u16_at(inode, 26)))
    } else {
        (u32_at(inode, 24), u32_at(inode, 28))
    };
    ((mode, ids.0, ids.1), block)
}

fn temp_files(store: &Store) -> usize {
    std::fs::read_dir(store.root().join("images/tmp"))
        .unwrap()
        .count()
}

#[test]
#[ignore = "needs the bundled lib/; run with `just it`"]
fn a_tar_without_a_root_entry_gets_a_root_owned_root() {
    let (_dir, store) = store();
    let images = store.layer_images();
    let layer = tar(&[
        ("app", tar::EntryType::Directory, 0o755, 1000),
        ("app/x", tar::EntryType::Regular, 0o644, 1000),
    ]);
    let diff_id = sha256(&layer);
    let path = images.ensure(&diff_id, || Ok(Cursor::new(layer))).unwrap();
    assert_eq!(path, images.path(&diff_id).unwrap());
    // Without the prepended entry the root would be the test user's, 0777.
    assert_eq!(root_inode(&path), ((0o40755, 0, 0), 4096));
}

#[test]
#[ignore = "needs the bundled lib/; run with `just it`"]
fn a_tar_root_entry_wins_over_the_default() {
    let (_dir, store) = store();
    let images = store.layer_images();
    let layer = tar(&[
        ("./", tar::EntryType::Directory, 0o700, 5),
        ("x", tar::EntryType::Regular, 0o644, 0),
    ]);
    let diff_id = sha256(&layer);
    let path = images.ensure(&diff_id, || Ok(Cursor::new(layer))).unwrap();
    assert_eq!(root_inode(&path).0, (0o40700, 5, 5));
}

#[test]
#[ignore = "needs the bundled lib/; run with `just it`"]
fn equal_tars_give_equal_images_and_present_images_are_reused() {
    let layer = tar(&[("x", tar::EntryType::Regular, 0o4755, 0)]);
    let diff_id = sha256(&layer);
    let (_a, a) = store();
    let (_b, b) = store();
    let first = a
        .layer_images()
        .ensure(&diff_id, || Ok(Cursor::new(layer.clone())))
        .unwrap();
    let second = b
        .layer_images()
        .ensure(&diff_id, || Ok(Cursor::new(layer)))
        .unwrap();
    assert_eq!(
        std::fs::read(&first).unwrap(),
        std::fs::read(&second).unwrap()
    );
    let again = a
        .layer_images()
        .ensure::<Cursor<Vec<u8>>>(&diff_id, || panic!("the image exists"))
        .unwrap();
    assert_eq!(again, first);
}

#[test]
#[ignore = "needs the bundled lib/; run with `just it`"]
fn data_after_the_end_of_the_archive_is_part_of_the_digest() {
    let (_dir, store) = store();
    // The converter may stop reading at the end-of-archive blocks; the
    // digest still covers every byte, as a diff_id does.
    let mut layer = tar(&[("x", tar::EntryType::Regular, 0o644, 0)]);
    layer.extend(std::iter::repeat_n(0, 4 << 20));
    let diff_id = sha256(&layer);
    let path = store
        .layer_images()
        .ensure(&diff_id, || Ok(Cursor::new(layer)))
        .unwrap();
    assert!(path.is_file());
}

#[test]
#[ignore = "needs the bundled lib/; run with `just it`"]
fn nothing_is_stored_for_a_wrong_diff_id_or_a_broken_tar() {
    let (_dir, store) = store();
    let images = store.layer_images();
    let layer = tar(&[("x", tar::EntryType::Regular, 0o644, 0)]);
    let claimed = sha256(b"another layer");
    let err = images
        .ensure(&claimed, || Ok(Cursor::new(layer)))
        .unwrap_err();
    assert!(format!("{err:#}").contains("its tar has digest"), "{err:#}");
    assert!(!images.path(&claimed).unwrap().exists());

    let garbage = b"this is not a tar archive at all, not even close".repeat(20);
    let diff_id = sha256(&garbage);
    let err = images
        .ensure(&diff_id, || Ok(Cursor::new(garbage)))
        .unwrap_err();
    assert!(
        format!("{err:#}").contains(&format!("converting layer {diff_id}")),
        "{err:#}"
    );
    assert!(!images.path(&diff_id).unwrap().exists());
    assert_eq!(temp_files(&store), 0, "partial images are removed");
}
