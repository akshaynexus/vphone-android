//! `vphone_apfs` (shipped as `libvphone_apfs.so`): the command-line front end.
//! See `usage` for the commands. Exit status is 0 on success, 1 on failure
//! with one `error: ...` line on stderr.

use serde_json::json;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use vphone_apfs::ops::{apply_meta_line, copy_in, extract, inject_ramdisk, walk_tree, MetaOverride};
use vphone_apfs::write::Attrs;
use vphone_apfs::{Container, Error, FileKind, Image, MutVolume, Result, Volume, VolumeSelector};

fn usage() -> ! {
    eprint!(
        "usage: vphone_apfs COMMAND IMAGE [ARGS...] [--vol N|--role ROLE|--name NAME] [--snapshot NAME]\n\
Read:\n\
  info IMG\n  ls IMG PATH [-R] [--json]\n  stat IMG PATH\n  cat IMG PATH\n  readxattr IMG PATH NAME\n\
  walk IMG [SRC]\n  extract IMG SRC DESTDIR [--manifest FILE]\n\
Write (unsealed volumes only):\n\
  write IMG GUESTPATH HOSTFILE [--mode 0755] [--uid N] [--gid N] [--keep-metadata]\n\
  copy-in IMG HOSTPATH GUESTDIR [--owner U:G] [--meta FILE]\n\
  mkdir IMG PATH [-p] [--mode M] [--uid U] [--gid G]\n\
  rm IMG PATH [-r] [-f]\n  chmod IMG PATH MODE\n  chown IMG PATH U:G\n  symlink IMG TARGET LINK\n\
  link IMG EXISTING NEW\n  rename IMG OLD NEW\n  setxattr IMG PATH NAME HOSTFILE\n  rmxattr IMG PATH NAME\n\
  settimes IMG PATH MTIME_NS ATIME_NS\n  inject-ramdisk IMG STAGEDIR\n"
    );
    std::process::exit(2);
}

/// Simple option parsing: flags and value options are pulled out wherever
/// they appear; what remains are the positional arguments.
struct Args {
    pos: Vec<String>,
    opts: Vec<(String, Option<String>)>,
}

impl Args {
    fn parse(raw: &[String]) -> Args {
        const VALUE_OPTS: [&str; 13] = [
            "--vol", "--role", "--name", "--snapshot", "--mode", "--uid", "--gid", "--owner", "--meta",
            "--manifest", "--offset", "--xattr-of", "--chunk",
        ];
        let mut pos = Vec::new();
        let mut opts = Vec::new();
        let mut i = 0;
        while i < raw.len() {
            let a = &raw[i];
            if VALUE_OPTS.contains(&a.as_str()) {
                let v = raw.get(i + 1).cloned();
                if v.is_none() {
                    usage();
                }
                opts.push((a.clone(), v));
                i += 2;
            } else if a.starts_with('-') && a.len() > 1 && !a.chars().nth(1).unwrap().is_ascii_digit() {
                opts.push((a.clone(), None));
                i += 1;
            } else {
                pos.push(a.clone());
                i += 1;
            }
        }
        Args { pos, opts }
    }

    fn val(&self, k: &str) -> Option<&str> {
        self.opts.iter().find(|(n, _)| n == k).and_then(|(_, v)| v.as_deref())
    }

    fn flag(&self, k: &str) -> bool {
        self.opts.iter().any(|(n, _)| n == k)
    }

    fn selector(&self) -> Result<VolumeSelector> {
        Ok(VolumeSelector {
            index: self.val("--vol").map(|v| parse_num(v, "volume")).transpose()?.map(|v| v as usize),
            role: self.val("--role").map(str::to_string),
            name: self.val("--name").map(str::to_string),
            snapshot: self.val("--snapshot").map(str::to_string),
        })
    }

    fn attrs(&self) -> Result<Attrs> {
        Ok(Attrs {
            mode: self.val("--mode").map(parse_mode).transpose()?,
            uid: self.val("--uid").map(|v| parse_num(v, "uid")).transpose()?.map(|v| v as u32),
            gid: self.val("--gid").map(|v| parse_num(v, "gid")).transpose()?.map(|v| v as u32),
            ..Default::default()
        })
    }
}

fn parse_num(s: &str, what: &str) -> Result<u64> {
    s.parse().map_err(|_| Error::Invalid(format!("bad {what}: '{s}'")))
}

fn parse_mode(s: &str) -> Result<u16> {
    let t = s.strip_prefix("0o").unwrap_or(s);
    u16::from_str_radix(t, 8).map_err(|_| Error::Invalid(format!("bad mode '{s}' (octal)")))
}

fn parse_owner(s: &str) -> Result<(u32, u32)> {
    let (u, g) = s.split_once(':').ok_or_else(|| Error::Invalid(format!("bad owner '{s}' (want U:G)")))?;
    Ok((parse_num(u, "uid")? as u32, parse_num(g, "gid")? as u32))
}

fn uuid(b: &[u8; 16]) -> String {
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn ls_entry(v: &Volume<'_>, path: &str, recursive: bool, json: bool, out: &mut dyn Write) -> Result<()> {
    let ino = v.lookup(path)?;
    if ino.kind() != FileKind::Dir {
        print_entry(v, path, &ino, json, out)?;
        return Ok(());
    }
    let mut entries = v.read_dir(ino.ino)?;
    entries.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    let base = path.trim_end_matches('/');
    for de in entries {
        let p = format!("{base}/{}", de.name);
        let i = v.inode(de.ino)?;
        print_entry(v, &p, &i, json, out)?;
        if recursive && i.kind() == FileKind::Dir {
            ls_entry(v, &p, true, json, out)?;
        }
    }
    Ok(())
}

fn print_entry(v: &Volume<'_>, path: &str, i: &vphone_apfs::Inode, json: bool, out: &mut dyn Write) -> Result<()> {
    let size = if i.kind() == FileKind::File { v.file_size(i)? } else { 0 };
    if json {
        writeln!(out, "{}", serde_json::json!({"path": path, "kind": i.kind().as_str(), "mode": i.mode & 0o7777, "uid": i.uid, "gid": i.gid, "size": size}))?;
    } else {
        writeln!(out, "{} {:o} {}:{} {:>10} {path}", i.kind().as_str(), i.mode & 0o7777, i.uid, i.gid, size)?;
    }
    Ok(())
}

fn with_volume<T>(a: &Args, f: impl FnOnce(&Volume<'_>) -> Result<T>) -> Result<T> {
    let img = PathBuf::from(&a.pos[1]);
    let c = Container::open(Image::open(&img, false)?)?;
    let v = c.open_volume(&a.selector()?)?;
    f(&v)
}

fn with_mut(a: &Args, f: impl FnOnce(&mut MutVolume) -> Result<()>) -> Result<()> {
    let mut v = MutVolume::open(Path::new(&a.pos[1]), &a.selector()?)?;
    f(&mut v)?;
    v.commit()
}

fn run(a: &Args) -> Result<()> {
    let cmd = a.pos[0].as_str();
    let arg = |n: usize| -> &str {
        match a.pos.get(n) {
            Some(s) => s,
            None => usage(),
        }
    };
    let out = std::io::stdout();
    let mut out = BufWriter::new(out.lock());
    match cmd {
        "info" => {
            let img = PathBuf::from(arg(1));
            let image = Image::open(&img, false)?;
            let wrapper = image.wrapper;
            let c = Container::open(image)?;
            let vols = c.volumes()?;
            let list: Vec<_> = vols
                .iter()
                .map(|v| {
                    let snaps = c
                        .open_volume(&VolumeSelector { index: Some(v.index), ..Default::default() })
                        .map(|o| o.info.snapshots)
                        .unwrap_or_default();
                    json!({"index": v.index, "name": v.name, "uuid": uuid(&v.uuid), "role": v.role_name,
                           "sealed": v.sealed, "encrypted": v.encrypted,
                           "incompat": v.incompat, "rocompat": v.rocompat, "snapshots": snaps})
                })
                .collect();
            writeln!(out, "{}", json!({"wrapper": wrapper, "block_size": c.block_size,
                "block_count": c.block_count, "xid": c.xid, "volumes": list}))?;
        }
        "ls" => with_volume(a, |v| ls_entry(v, arg(2), a.flag("-R"), a.flag("--json"), &mut out))?,
        "stat" => with_volume(a, |v| {
            let i = v.lookup(arg(2))?;
            let xattrs: Vec<String> = v.xattrs(i.ino)?.into_iter().map(|x| x.name).collect();
            let symlink = if i.kind() == FileKind::Symlink { Some(v.read_symlink(&i)?) } else { None };
            let size = if i.kind() == FileKind::File { v.file_size(&i)? } else { 0 };
            writeln!(out, "{}", json!({"path": arg(2), "kind": i.kind().as_str(), "mode": i.mode & 0o7777,
                "uid": i.uid, "gid": i.gid, "flags": i.bsd_flags, "nlink": i.nlink(), "size": size,
                "ino": i.ino, "btime_ns": i.create_time, "mtime_ns": i.mod_time, "ctime_ns": i.change_time,
                "atime_ns": i.access_time, "xattrs": xattrs, "symlink": symlink, "compressed": i.is_compressed()}))?;
            Ok(())
        })?,
        "cat" => with_volume(a, |v| {
            let i = v.lookup(arg(2))?;
            v.write_file_to(&i, &mut out)
        })?,
        "readxattr" => with_volume(a, |v| {
            let i = v.lookup(arg(2))?;
            let d = v
                .getxattr(i.ino, arg(3))?
                .ok_or_else(|| Error::NotFound(format!("xattr {}", arg(3))))?;
            out.write_all(&d)?;
            Ok(())
        })?,
        "walk" => with_volume(a, |v| {
            walk_tree(v, a.pos.get(2).map(String::as_str).unwrap_or("/"), &mut |e| {
                writeln!(out, "{}", e.to_json())?;
                Ok(())
            })
        })?,
        "extract" => with_volume(a, |v| {
            let dest = PathBuf::from(arg(3));
            let mut mf = match a.val("--manifest") {
                Some(p) => Some(BufWriter::new(std::fs::File::create(p)?)),
                None => None,
            };
            let n = extract(v, arg(2), &dest, mf.as_mut().map(|m| m as &mut dyn Write))?;
            if let Some(mut m) = mf {
                m.flush()?;
            }
            writeln!(out, "extracted {n} entries")?;
            Ok(())
        })?,
        "write" => with_mut(a, |v| {
            let data = std::fs::read(arg(3))?;
            if a.flag("--keep-metadata") {
                v.replace_keep_metadata(arg(2), &data)?;
            } else {
                let mut attrs = a.attrs()?;
                if attrs.mode.is_none() {
                    attrs.mode = Some(0o644);
                }
                v.create_file(arg(2), &data, &attrs)?;
            }
            Ok(())
        })?,
        "copy-in" => with_mut(a, |v| {
            let owner = a.val("--owner").map(parse_owner).transpose()?;
            let meta = match a.val("--meta") {
                Some(p) => {
                    let mut m = MetaOverride::default();
                    for line in std::fs::read_to_string(p)?.lines().filter(|l| !l.trim().is_empty()) {
                        apply_meta_line(&mut m, line)?;
                    }
                    Some(m)
                }
                None => None,
            };
            let n = copy_in(v, Path::new(arg(2)), arg(3), owner, meta.as_ref())?;
            writeln!(out, "copied {n} entries")?;
            Ok(())
        })?,
        "mkdir" => with_mut(a, |v| {
            let mut attrs = a.attrs()?;
            if attrs.mode.is_none() {
                attrs.mode = Some(0o755);
            }
            v.mkdir(arg(2), &attrs, a.flag("-p"))?;
            Ok(())
        })?,
        "rm" => with_mut(a, |v| {
            let force = a.flag("-f");
            if force && !v.exists(arg(2))? {
                return Ok(());
            }
            if a.flag("-r") {
                v.remove_all(arg(2))
            } else {
                let i = v.lookup(arg(2))?;
                if i.kind() == FileKind::Dir && !a.flag("-d") {
                    return Err(Error::Invalid(format!("{}: is a directory (use -r)", arg(2))));
                }
                v.remove(arg(2))
            }
        })?,
        "chmod" => with_mut(a, |v| {
            v.set_attrs(arg(2), &Attrs { mode: Some(parse_mode(arg(3))?), ..Default::default() })
        })?,
        "chown" => with_mut(a, |v| {
            let (u, g) = parse_owner(arg(3))?;
            v.set_attrs(arg(2), &Attrs { uid: Some(u), gid: Some(g), ..Default::default() })
        })?,
        "symlink" => with_mut(a, |v| {
            v.symlink(arg(2), arg(3), &Attrs { uid: Some(0), gid: Some(0), ..Default::default() })?;
            Ok(())
        })?,
        "link" => with_mut(a, |v| v.link(arg(2), arg(3)))?,
        "rename" => with_mut(a, |v| v.rename(arg(2), arg(3)))?,
        "setxattr" => with_mut(a, |v| {
            let d = std::fs::read(arg(4))?;
            v.setxattr(arg(2), arg(3), &d)
        })?,
        "rmxattr" => with_mut(a, |v| v.rmxattr(arg(2), arg(3)))?,
        "settimes" => with_mut(a, |v| {
            v.set_attrs(
                arg(2),
                &Attrs {
                    mtime_ns: Some(parse_num(arg(3), "mtime")?),
                    atime_ns: Some(parse_num(arg(4), "atime")?),
                    ..Default::default()
                },
            )
        })?,
        "inject-ramdisk" => with_mut(a, |v| {
            let n = inject_ramdisk(v, Path::new(arg(2)))?;
            writeln!(out, "injected {n} entries")?;
            Ok(())
        })?,
        _ => usage(),
    }
    out.flush()?;
    Ok(())
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.len() < 2 {
        usage();
    }
    let a = Args::parse(&raw);
    if a.pos.len() < 2 {
        usage();
    }
    if let Err(e) = run(&a) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
