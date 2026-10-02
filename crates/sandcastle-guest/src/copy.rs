//! Docker `COPY` from the build context into an image root. Paths resolve
//! as if each root were `/`. Uses only std, so the rules test on any host.

use std::ffi::OsString;
use std::fs::{self, File, FileTimes, Metadata, Permissions};
use std::io::{self, ErrorKind};
use std::os::unix::fs::{MetadataExt, PermissionsExt, lchown, symlink};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

/// Most symlinks followed while resolving one path (Linux's MAXSYMLINKS).
const MAX_LINKS: usize = 40;

/// Resolves `path` inside `root` as if `root` were `/`: `..` stops at the
/// root and absolute link targets restart from it. Components that do not
/// exist are kept as written.
pub fn resolve_in_root(root: &Path, path: &Path) -> Result<PathBuf> {
    let mut resolved = PathBuf::new();
    let mut pending: Vec<OsString> = parts(path);
    pending.reverse();
    let mut links = 0;
    while let Some(name) = pending.pop() {
        if name == ".." {
            resolved.pop();
            continue;
        }
        let candidate = resolved.join(&name);
        match fs::symlink_metadata(root.join(&candidate)) {
            Ok(meta) if meta.file_type().is_symlink() => {
                links += 1;
                ensure!(
                    links <= MAX_LINKS,
                    "too many levels of symbolic links in {}",
                    path.display()
                );
                let target = fs::read_link(root.join(&candidate))?;
                if target.is_absolute() {
                    resolved = PathBuf::new();
                }
                let mut target_parts = parts(&target);
                target_parts.reverse();
                pending.extend(target_parts);
            }
            Ok(_) => resolved = candidate,
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                resolved = candidate;
            }
            Err(e) => return Err(e).with_context(|| format!("resolving {}", path.display())),
        }
    }
    Ok(root.join(resolved))
}

/// Normal components and `..`, in order; `.` and the root are dropped.
fn parts(path: &Path) -> Vec<OsString> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_os_string()),
            Component::ParentDir => Some("..".into()),
            _ => None,
        })
        .collect()
}

fn has_meta(s: &str) -> bool {
    s.contains(['*', '?', '[', '\\'])
}

/// Go's `filepath.Match` for one path component: `*`, `?`, `[...]` with
/// ranges and `^` negation, and `\` escapes.
pub fn matches(pattern: &str, name: &str) -> Result<bool> {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    match_at(&p, &n).with_context(|| format!("bad pattern {pattern:?}"))
}

fn match_at(p: &[char], n: &[char]) -> Result<bool> {
    let Some((&first, rest)) = p.split_first() else {
        return Ok(n.is_empty());
    };
    match first {
        '*' => {
            for skip in 0..=n.len() {
                if match_at(rest, &n[skip..])? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        '?' => Ok(!n.is_empty() && match_at(rest, &n[1..])?),
        '[' => {
            let (hit, after) = class(rest, n.first().copied())?;
            Ok(hit && match_at(after, &n[1..])?)
        }
        '\\' => {
            let (&c, rest) = rest.split_first().context("trailing backslash")?;
            Ok(n.first() == Some(&c) && match_at(rest, &n[1..])?)
        }
        c => Ok(n.first() == Some(&c) && match_at(rest, &n[1..])?),
    }
}

/// Parses a character class after `[`; returns whether `c` is in it and the
/// pattern after the closing `]`.
fn class(mut p: &[char], c: Option<char>) -> Result<(bool, &[char])> {
    let negate = p.first() == Some(&'^');
    if negate {
        p = &p[1..];
    }
    let mut hit = false;
    let mut first = true;
    loop {
        let Some((&ch, rest)) = p.split_first() else {
            bail!("missing ]");
        };
        if ch == ']' && !first {
            p = rest;
            break;
        }
        first = false;
        let (lo, rest) = class_char(p)?;
        p = rest;
        let hi = if p.first() == Some(&'-') && p.get(1) != Some(&']') {
            let (hi, rest) = class_char(&p[1..])?;
            p = rest;
            hi
        } else {
            lo
        };
        if c.is_some_and(|c| lo <= c && c <= hi) {
            hit = true;
        }
    }
    Ok((c.is_some() && hit != negate, p))
}

fn class_char(p: &[char]) -> Result<(char, &[char])> {
    match p {
        ['\\', c, rest @ ..] => Ok((*c, rest)),
        [c, rest @ ..] if *c != ']' => Ok((*c, rest)),
        _ => bail!("bad character class"),
    }
}

/// A resolved source path and the name it is copied under, which is the
/// last component before symlinks were followed.
type Source = (PathBuf, Option<OsString>);

/// The last real component of `path`, if any.
fn last_name(path: &Path) -> Option<OsString> {
    parts(path).pop().filter(|n| n != "..")
}

/// Sources in the context for one COPY argument, sorted.
fn expand_source(ctx: &Path, source: &str) -> Result<Vec<Source>> {
    if !has_meta(source) {
        let path = resolve_in_root(ctx, Path::new(source))?;
        ensure!(
            fs::symlink_metadata(&path).is_ok(),
            "{source}: not found in the build context"
        );
        return Ok(vec![(path, last_name(Path::new(source)))]);
    }
    let mut current = vec![PathBuf::new()];
    for part in parts(Path::new(source)) {
        let part_str = part.to_string_lossy();
        let mut next = Vec::new();
        for base in &current {
            if part == ".." {
                let mut up = base.clone();
                up.pop();
                next.push(up);
            } else if has_meta(&part_str) {
                let Ok(entries) = fs::read_dir(resolve_in_root(ctx, base)?) else {
                    continue;
                };
                let mut names: Vec<OsString> = entries
                    .filter_map(|e| e.ok().map(|e| e.file_name()))
                    .collect();
                names.sort();
                for name in names {
                    if let Some(s) = name.to_str()
                        && matches(&part_str, s)?
                    {
                        next.push(base.join(&name));
                    }
                }
            } else {
                next.push(base.join(&part));
            }
        }
        current = next;
    }
    let mut out = Vec::new();
    for rel in current {
        let path = resolve_in_root(ctx, &rel)?;
        if fs::symlink_metadata(&path).is_ok() {
            out.push((path, last_name(&rel)));
        }
    }
    ensure!(
        !out.is_empty(),
        "{source}: no files in the build context match"
    );
    Ok(out)
}

/// One COPY step: `ctx` is the build context, `root` the image root.
pub struct Copy<'a> {
    pub ctx: &'a Path,
    pub root: &'a Path,
    pub workdir: &'a str,
    /// Owner for everything written; `None` keeps the process's own ids.
    pub owner: Option<(u32, u32)>,
}

impl Copy<'_> {
    pub fn run(&self, sources: &[String], dest: &str) -> Result<()> {
        let mut srcs = Vec::new();
        for source in sources {
            srcs.extend(expand_source(self.ctx, source)?);
        }
        let into_dir = dest.ends_with('/') || dest == "." || dest.ends_with("/.") || srcs.len() > 1;
        let target = resolve_in_root(self.root, &Path::new(self.workdir).join(dest))?;
        for (src, name) in &srcs {
            if fs::metadata(src)?.is_dir() {
                self.mkdir_p(&target)?;
                self.copy_children(src, &target)?;
            } else {
                let file_dest = if into_dir || target.is_dir() {
                    self.mkdir_p(&target)?;
                    let name = name.as_deref().or(src.file_name());
                    target.join(name.context("source has no file name")?)
                } else {
                    self.mkdir_p(target.parent().context("destination has no parent")?)?;
                    target.clone()
                };
                self.copy_entry(src, &file_dest)?;
            }
        }
        Ok(())
    }

    /// Creates the step's working directory like `mkdir -p`.
    pub fn ensure_workdir(&self) -> Result<()> {
        self.mkdir_p(&resolve_in_root(self.root, Path::new(self.workdir))?)
    }

    /// `dir` is already resolved inside the root, so every existing
    /// component is a real directory and missing ones are created `0755`.
    fn mkdir_p(&self, dir: &Path) -> Result<()> {
        let rel = dir
            .strip_prefix(self.root)
            .context("destination leaves the image root")?;
        let mut cur = self.root.to_path_buf();
        for component in rel.components() {
            cur.push(component);
            match fs::symlink_metadata(&cur) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => bail!("{} is not a directory", self.shown(&cur)),
                Err(e) if e.kind() == ErrorKind::NotFound => {
                    fs::create_dir(&cur)?;
                    self.chown(&cur)?;
                    fs::set_permissions(&cur, Permissions::from_mode(0o755))?;
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    fn copy_children(&self, src_dir: &Path, dest_dir: &Path) -> Result<()> {
        let mut names: Vec<OsString> = fs::read_dir(src_dir)?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<io::Result<_>>()?;
        names.sort();
        for name in names {
            self.copy_entry(&src_dir.join(&name), &dest_dir.join(&name))?;
        }
        Ok(())
    }

    fn copy_entry(&self, src: &Path, dst: &Path) -> Result<()> {
        let meta = fs::symlink_metadata(src)?;
        let ft = meta.file_type();
        let mut created = true;
        match fs::symlink_metadata(dst) {
            Ok(existing) if existing.is_dir() && ft.is_dir() => created = false,
            Ok(existing) if existing.is_dir() => bail!(
                "cannot replace directory {} with a non-directory",
                self.shown(dst)
            ),
            Ok(_) => fs::remove_file(dst)?,
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        if ft.is_dir() {
            if created {
                fs::create_dir(dst)?;
            }
            self.copy_children(src, dst)?;
            if created {
                self.finish(dst, &meta)?;
            }
        } else if ft.is_file() {
            let mut from = File::open(src)?;
            let mut to = File::create_new(dst)?;
            io::copy(&mut from, &mut to).with_context(|| format!("copying {}", src.display()))?;
            drop(to);
            self.finish(dst, &meta)?;
        } else if ft.is_symlink() {
            symlink(fs::read_link(src)?, dst)?;
            self.chown(dst)?;
        } else {
            eprintln!("sandcastle-guest: skipping special file {}", src.display());
        }
        Ok(())
    }

    /// Owner first (chown clears setuid bits), then mtime, then mode.
    fn finish(&self, path: &Path, meta: &Metadata) -> Result<()> {
        self.chown(path)?;
        File::open(path)?.set_times(FileTimes::new().set_modified(meta.modified()?))?;
        fs::set_permissions(path, Permissions::from_mode(meta.mode() & 0o7777))?;
        Ok(())
    }

    fn chown(&self, path: &Path) -> Result<()> {
        if let Some((uid, gid)) = self.owner {
            lchown(path, Some(uid), Some(gid))?;
        }
        Ok(())
    }

    fn shown(&self, path: &Path) -> String {
        Path::new("/")
            .join(path.strip_prefix(self.root).unwrap_or(path))
            .display()
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::time::{Duration, SystemTime};

    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        ctx: PathBuf,
        root: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let ctx = dir.path().join("ctx");
        let root = dir.path().join("root");
        fs::create_dir_all(ctx.join("tree/sub")).unwrap();
        fs::write(ctx.join("hello.txt"), "hi").unwrap();
        fs::write(ctx.join("one.conf"), "1").unwrap();
        fs::write(ctx.join("two.conf"), "2").unwrap();
        fs::write(ctx.join("tree/a.txt"), "a").unwrap();
        fs::write(ctx.join("tree/sub/b.txt"), "b").unwrap();
        symlink("a.txt", ctx.join("tree/link")).unwrap();
        fs::create_dir_all(root.join("usr/bin")).unwrap();
        symlink("usr/bin", root.join("bin")).unwrap();
        Fixture {
            _dir: dir,
            ctx,
            root,
        }
    }

    fn copy<'a>(f: &'a Fixture, workdir: &'a str) -> Copy<'a> {
        Copy {
            ctx: &f.ctx,
            root: &f.root,
            workdir,
            owner: None,
        }
    }

    #[test]
    fn resolve_in_root_keeps_links_inside_root() {
        let f = fixture();
        symlink("/usr", f.root.join("abs")).unwrap();
        assert_eq!(
            resolve_in_root(&f.root, Path::new("/abs/bin/x")).unwrap(),
            f.root.join("usr/bin/x")
        );
        assert_eq!(
            resolve_in_root(&f.root, Path::new("../../bin")).unwrap(),
            f.root.join("usr/bin")
        );
        symlink("loop", f.root.join("loop")).unwrap();
        assert!(resolve_in_root(&f.root, Path::new("loop")).is_err());
    }

    #[test]
    fn glob_matches_like_go() {
        for (pattern, name, want) in [
            ("*.txt", "a.txt", true),
            ("*.txt", "a.md", false),
            ("*", ".hidden", true),
            ("a?c", "abc", true),
            ("[a-c]x", "bx", true),
            ("[^a-c]x", "bx", false),
            ("[^a-c]x", "dx", true),
            ("\\*", "*", true),
            ("\\*", "a", false),
        ] {
            assert_eq!(matches(pattern, name).unwrap(), want, "{pattern} vs {name}");
        }
        assert!(matches("[a-", "a").is_err());
    }

    #[test]
    fn file_into_dir_and_file_rename() {
        let f = fixture();
        copy(&f, "/").run(&["hello.txt".into()], "/app/").unwrap();
        assert_eq!(
            fs::read_to_string(f.root.join("app/hello.txt")).unwrap(),
            "hi"
        );
        copy(&f, "/")
            .run(&["hello.txt".into()], "/renamed.txt")
            .unwrap();
        assert_eq!(
            fs::read_to_string(f.root.join("renamed.txt")).unwrap(),
            "hi"
        );
    }

    #[test]
    fn dir_source_copies_contents_and_keeps_nested_links() {
        let f = fixture();
        copy(&f, "/").run(&["tree".into()], "/data").unwrap();
        assert_eq!(fs::read_to_string(f.root.join("data/a.txt")).unwrap(), "a");
        assert_eq!(
            fs::read_to_string(f.root.join("data/sub/b.txt")).unwrap(),
            "b"
        );
        assert_eq!(
            fs::read_link(f.root.join("data/link")).unwrap(),
            Path::new("a.txt")
        );
        assert!(!f.root.join("data/tree").exists());
    }

    #[test]
    fn wildcard_multiple_matches_go_into_dir() {
        let f = fixture();
        copy(&f, "/").run(&["*.conf".into()], "/etc/demo").unwrap();
        assert_eq!(
            fs::read_to_string(f.root.join("etc/demo/one.conf")).unwrap(),
            "1"
        );
        assert_eq!(
            fs::read_to_string(f.root.join("etc/demo/two.conf")).unwrap(),
            "2"
        );
    }

    #[test]
    fn relative_dest_uses_workdir() {
        let f = fixture();
        copy(&f, "/srv/app")
            .run(&["hello.txt".into()], "./")
            .unwrap();
        assert!(f.root.join("srv/app/hello.txt").is_file());
    }

    #[test]
    fn dest_through_image_symlink_stays_in_root() {
        let f = fixture();
        copy(&f, "/").run(&["hello.txt".into()], "/bin/").unwrap();
        assert!(f.root.join("usr/bin/hello.txt").is_file());
    }

    #[test]
    fn source_symlink_outside_context_resolves_inside_it() {
        let f = fixture();
        symlink("/hello.txt", f.ctx.join("abs-link")).unwrap();
        copy(&f, "/").run(&["abs-link".into()], "/got").unwrap();
        assert_eq!(fs::read_to_string(f.root.join("got")).unwrap(), "hi");
        let err = copy(&f, "/")
            .run(&["../../etc/passwd".into()], "/x")
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("not found in the build context"),
            "{err:#}"
        );
    }

    #[test]
    fn missing_sources_are_errors() {
        let f = fixture();
        let err = copy(&f, "/").run(&["nope.txt".into()], "/x").unwrap_err();
        assert!(
            format!("{err:#}").contains("nope.txt: not found in the build context"),
            "{err:#}"
        );
        let err = copy(&f, "/").run(&["*.rs".into()], "/x/").unwrap_err();
        assert!(
            format!("{err:#}").contains("*.rs: no files in the build context match"),
            "{err:#}"
        );
    }

    #[test]
    fn mode_and_mtime_are_preserved() {
        let f = fixture();
        let src = f.ctx.join("run.sh");
        fs::write(&src, "#!/bin/sh").unwrap();
        fs::set_permissions(&src, Permissions::from_mode(0o750)).unwrap();
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        File::options()
            .write(true)
            .open(&src)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        copy(&f, "/").run(&["run.sh".into()], "/run.sh").unwrap();
        let meta = fs::metadata(f.root.join("run.sh")).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o750);
        assert_eq!(meta.modified().unwrap(), mtime);
    }

    #[test]
    fn overwrites_files_but_not_dirs_and_keeps_existing_dir_metadata() {
        let f = fixture();
        fs::write(f.root.join("renamed.txt"), "old").unwrap();
        copy(&f, "/")
            .run(&["hello.txt".into()], "/renamed.txt")
            .unwrap();
        assert_eq!(
            fs::read_to_string(f.root.join("renamed.txt")).unwrap(),
            "hi"
        );

        fs::create_dir_all(f.root.join("data/a.txt")).unwrap();
        let err = copy(&f, "/").run(&["tree".into()], "/data").unwrap_err();
        assert!(
            format!("{err:#}").contains("cannot replace directory"),
            "{err:#}"
        );

        fs::create_dir(f.root.join("keep")).unwrap();
        fs::set_permissions(f.root.join("keep"), Permissions::from_mode(0o700)).unwrap();
        copy(&f, "/").run(&["hello.txt".into()], "/keep/").unwrap();
        assert_eq!(
            fs::metadata(f.root.join("keep")).unwrap().mode() & 0o7777,
            0o700
        );
    }

    #[test]
    fn followed_symlink_sources_keep_the_link_name() {
        let f = fixture();
        symlink("/hello.txt", f.ctx.join("abs-link")).unwrap();
        copy(&f, "/").run(&["abs-link".into()], "/app/").unwrap();
        assert_eq!(
            fs::read_to_string(f.root.join("app/abs-link")).unwrap(),
            "hi"
        );
        assert!(!f.root.join("app/hello.txt").exists());

        symlink("hello.txt", f.ctx.join("three.conf")).unwrap();
        copy(&f, "/").run(&["thr*.conf".into()], "/etc/").unwrap();
        assert_eq!(
            fs::read_to_string(f.root.join("etc/three.conf")).unwrap(),
            "hi"
        );
    }
}
