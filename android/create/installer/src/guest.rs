//! Steps that talk to a running guest through vphoned: Setup Assistant,
//! unlock, Auto-Lock, the guest device tree's scale, and the 1x boot logo.
//! Ports of tools/auto-lock-never.py, guest-dt-scale.py, guest-1x-logo.py and
//! `setup/create-vm.sh`'s first boot.

use crate::bail;
use crate::der;
use crate::error::{Error, Result};
use crate::host::Host;
use base64::Engine;
use image::imageops::{self, FilterType};
use image::{ImageFormat, RgbaImage};
use serde_json::{json, Value};
use std::io::{Cursor, Read};
use std::time::Duration;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
const NEVER: i64 = (1 << 31) - 1;
const LONG: Duration = Duration::from_secs(120);

const MOBILE_GESTALT_CACHE: &str = "/private/var/containers/Shared/SystemGroup/systemgroup.com.apple.mobilegestaltcache/Library/Caches/com.apple.MobileGestalt.plist";

fn rpc(host: &dyn Host, method: &str, params: Value) -> Result<Value> {
    host.vphoned_rpc(method, params, LONG)
        .map_err(|e| Error::msg(format!("{method}: {e}")))
}

/// `true` when vphoned reports the data migration finished.
pub fn data_migration_done(host: &dyn Host) -> bool {
    rpc(host, "setup.status", json!({}))
        .map(|r| r.get("data_migration_done").and_then(Value::as_bool) == Some(true))
        .unwrap_or(false)
}

pub fn skip_setup(host: &dyn Host) -> Result<Value> {
    rpc(host, "setup.skip", json!({"force": true}))
}

/// Up to 40 tries, 15 s apart, as `unlock_vm` does; false if it never unlocks.
pub fn unlock(host: &dyn Host) -> bool {
    for _ in 0..40 {
        if host.cancelled() {
            return false;
        }
        if rpc(host, "screen.unlock", json!({})).is_ok_and(|r| r.get("locked").and_then(Value::as_bool) == Some(false)) {
            return true;
        }
        std::thread::sleep(Duration::from_secs(15));
    }
    false
}

/// Set Auto-Lock to Never through ManagedConfiguration, as Settings does.
pub fn auto_lock_never(host: &dyn Host) -> Result<()> {
    let source = format!(
        r#"
ObjC.use("NSBundle").bundleWithPath_("/System/Library/PrivateFrameworks/ManagedConfiguration.framework").load();
var c = ObjC.use("MCProfileConnection").sharedConnection();
c.setValue_forSetting_(ObjC.use("NSNumber").numberWithInt_({NEVER}), "maxInactivity");
sleep(2);
print(c.effectiveValueForSetting_("maxInactivity"));
"#
    );
    let r = rpc(host, "script.eval", json!({"source": source, "timeout": 60}))?;
    if r.get("ok").and_then(Value::as_bool) != Some(true) {
        bail!("the Auto-Lock script failed: {} {}", r["error"], r["output"]);
    }
    let last = r
        .get("output")
        .and_then(Value::as_str)
        .and_then(|o| o.trim().lines().last())
        .and_then(|l| l.trim().parse::<i64>().ok());
    if last != Some(NEVER) {
        bail!("profiled ignored the Auto-Lock call: Auto-Lock is unchanged (is vphoned signed with ManagedConfiguration's entitlements?)");
    }
    Ok(())
}

fn files_list(host: &dyn Host, path: &str) -> Result<Vec<Value>> {
    let r = rpc(host, "files.list", json!({"path": path}))?;
    r.get("entries")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| Error::msg(format!("files.list {path}: no entries")))
}

fn files_read(host: &dyn Host, path: &str) -> Result<Vec<u8>> {
    let r = rpc(host, "files.read", json!({"path": path, "binary": true}))?;
    let content = r
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::msg(format!("files.read {path}: no content")))?;
    B64.decode(content).map_err(|e| Error::msg(format!("files.read {path}: {e}")))
}

fn files_write(host: &dyn Host, path: &str, data: &[u8]) -> Result<()> {
    rpc(host, "files.write", json!({"path": path, "content": B64.encode(data), "encoding": "base64"}))?;
    Ok(())
}

/// The offset and length of the device tree inside an IMG4 (or a bare IM4P).
pub fn dt_payload(img4: &[u8]) -> Result<(usize, usize)> {
    let outer = der::tlv(img4, 0)?;
    let mut element = der::tlv(img4, outer.start)?;
    if &img4[element.start..element.end] == b"IMG4" {
        let im4p = der::tlv(img4, element.end)?;
        element = der::tlv(img4, im4p.start)?;
    }
    for _ in 0..2 {
        element = der::tlv(img4, element.end)?;
    }
    let payload = der::tlv(img4, element.end)?;
    if payload.tag != 0x04 {
        bail!("unexpected IM4P layout: no payload");
    }
    if matches!(img4.get(payload.start..payload.start + 4), Some(b"bvx2" | b"bvx1" | b"bvxn")) {
        bail!("the tree is LZFSE-compressed; this patches only an uncompressed one");
    }
    Ok((payload.start, payload.end))
}

fn walk_dt(dt: &[u8], mut off: usize, path: &str, hits: &mut Vec<(String, usize)>) -> Result<usize> {
    if off + 8 > dt.len() {
        bail!("device tree is truncated");
    }
    let nprops = u32::from_le_bytes(dt[off..off + 4].try_into().unwrap());
    let nchild = u32::from_le_bytes(dt[off + 4..off + 8].try_into().unwrap());
    off += 8;
    let mut name = String::new();
    let mut props = Vec::new();
    for _ in 0..nprops {
        if off + 36 > dt.len() {
            bail!("device tree is truncated");
        }
        let key_end = dt[off..off + 32].iter().position(|&b| b == 0).unwrap_or(32);
        let key = String::from_utf8_lossy(&dt[off..off + key_end]).into_owned();
        let n = (u32::from_le_bytes(dt[off + 32..off + 36].try_into().unwrap()) & 0x7FFF_FFFF) as usize;
        if off + 36 + n > dt.len() {
            bail!("device tree is truncated");
        }
        if key == "name" {
            let value = &dt[off + 36..off + 36 + n];
            let end = value.iter().position(|&b| b == 0).unwrap_or(n);
            name = String::from_utf8_lossy(&value[..end]).into_owned();
        }
        props.push((key, off + 36, n));
        off += 36 + ((n + 3) & !3);
    }
    let here = if path.is_empty() { name } else { format!("{path}/{name}") };
    for (key, at, n) in props {
        if (key == "display-scale" || key == "artwork-scale-factor") && here.contains("/product") && n == 4 {
            hits.push((format!("{here}:{key}"), at));
        }
    }
    for _ in 0..nchild {
        off = walk_dt(dt, off, &here, hits)?;
    }
    Ok(off)
}

/// Set `scale` in an in-memory IMG4 device tree; the number of values set
/// (none means LLB alone sets this guest's scale).
pub fn set_dt_scale(img4: &mut [u8], scale: u32) -> Result<usize> {
    let (start, end) = dt_payload(img4)?;
    let mut hits = Vec::new();
    walk_dt(&img4[..end], start, "", &mut hits)?;
    for (_, at) in &hits {
        img4[*at..*at + 4].copy_from_slice(&scale.to_le_bytes());
    }
    Ok(hits.len())
}

/// `guest-dt-scale.py CID SCALE`, keeping the original beside it in the guest.
/// Returns false when the tree holds no scale (the LLB patch alone sets it).
pub fn guest_dt_scale(host: &dyn Host, scale: u32) -> Result<bool> {
    let boot: Vec<String> = files_list(host, "/private/preboot")?
        .iter()
        .filter(|e| e["type"] == "dir")
        .filter_map(|e| e["name"].as_str().filter(|n| n.len() == 96).map(str::to_owned))
        .collect();
    if boot.len() != 1 {
        bail!("expected one boot manifest directory in /private/preboot, found {boot:?}");
    }
    let dir = format!("/private/preboot/{}/usr/standalone/firmware", boot[0]);
    let path = format!("{dir}/devicetree.img4");
    let mut img4 = files_read(host, &path)?;
    let original = img4.clone();
    if set_dt_scale(&mut img4, scale)? == 0 {
        return Ok(false);
    }
    let names: Vec<String> = files_list(host, &dir)?
        .iter()
        .filter_map(|e| e["name"].as_str().map(str::to_owned))
        .collect();
    if !names.iter().any(|n| n == "devicetree.img4.before-scale") {
        files_write(host, &format!("{path}.before-scale"), &original)?;
    }
    files_write(host, &path, &img4)?;
    if files_read(host, &path)? != img4 {
        bail!("the device tree read back differs from what was written; the original is at {path}.before-scale");
    }
    let _ = rpc(host, "files.remove", json!({"path": MOBILE_GESTALT_CACHE}));
    Ok(true)
}

/// Remove MobileGestalt's cached scale (set-scale.sh's step 2).
pub fn clear_scale_cache(host: &dyn Host) {
    let _ = rpc(host, "files.remove", json!({"path": MOBILE_GESTALT_CACHE, "force": true}));
}

/// Decode a PNG, including iOS's CgBI variant (BGRA, premultiplied, raw deflate).
pub fn decode_png(data: &[u8]) -> Result<RgbaImage> {
    if data.get(12..16) != Some(b"CgBI") {
        let img = image::load_from_memory_with_format(data, ImageFormat::Png)
            .map_err(|e| Error::msg(format!("png: {e}")))?;
        return Ok(img.to_rgba8());
    }
    let (mut w, mut h, mut depth, mut ctype, mut interlace) = (0u32, 0u32, 0u8, 0u8, 0u8);
    let mut idat = Vec::new();
    let mut pos = 8;
    while pos + 12 <= data.len() {
        let n = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        let kind = &data[pos + 4..pos + 8];
        let body = data
            .get(pos + 8..pos + 8 + n)
            .ok_or_else(|| Error::msg("truncated PNG"))?;
        if kind == b"IHDR" && n >= 13 {
            w = u32::from_be_bytes(body[..4].try_into().unwrap());
            h = u32::from_be_bytes(body[4..8].try_into().unwrap());
            depth = body[8];
            ctype = body[9];
            interlace = body[12];
        } else if kind == b"IDAT" {
            idat.extend_from_slice(body);
        }
        pos += 12 + n;
    }
    if (depth, ctype, interlace) != (8, 6, 0) {
        bail!("unsupported CgBI PNG (depth {depth}, color type {ctype}, interlace {interlace})");
    }
    let mut raw = Vec::new();
    flate2::read::DeflateDecoder::new(&idat[..])
        .read_to_end(&mut raw)
        .map_err(|e| Error::msg(format!("CgBI inflate: {e}")))?;
    let stride = w as usize * 4;
    if raw.len() < (stride + 1) * h as usize {
        bail!("CgBI PNG is short");
    }
    let mut out = Vec::with_capacity(stride * h as usize);
    let mut prev = vec![0u8; stride];
    for y in 0..h as usize {
        let at = y * (stride + 1);
        let f = raw[at];
        let mut row = raw[at + 1..at + 1 + stride].to_vec();
        for i in 0..stride {
            let a = if i >= 4 { row[i - 4] } else { 0 } as i32;
            let b = prev[i] as i32;
            let c = if i >= 4 { prev[i - 4] } else { 0 } as i32;
            let add = match f {
                1 => a,
                2 => b,
                3 => (a + b) / 2,
                4 => {
                    let p = a + b - c;
                    let (pa, pb, pc) = ((p - a).abs(), (p - b).abs(), (p - c).abs());
                    if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    }
                }
                _ => 0,
            };
            row[i] = row[i].wrapping_add(add as u8);
        }
        out.extend_from_slice(&row);
        prev = row;
    }
    for px in out.chunks_exact_mut(4) {
        let (bl, g, r, al) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
        let un = |v: u32| if al != 0 { (v * 255 / al).min(255) as u8 } else { v as u8 };
        px[0] = un(r);
        px[1] = un(g);
        px[2] = un(bl);
    }
    RgbaImage::from_raw(w, h, out).ok_or_else(|| Error::msg("CgBI PNG size mismatch"))
}

/// Halve a @2x logo (Lanczos) and return it as a plain PNG.
pub fn half_size_png(data: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
    let img = decode_png(data)?;
    let (w, h) = ((img.width() / 2).max(1), (img.height() / 2).max(1));
    // Like Pillow, resample with premultiplied alpha so transparent pixels'
    // colour does not bleed into the edge of the logo.
    let mut img = img;
    for px in img.pixels_mut() {
        let a = px[3] as u32;
        for c in 0..3 {
            px[c] = ((px[c] as u32 * a + 127) / 255) as u8;
        }
    }
    let mut small = imageops::resize(&img, w, h, FilterType::Lanczos3);
    for px in small.pixels_mut() {
        let a = px[3] as u32;
        if a != 0 {
            for c in 0..3 {
                px[c] = ((px[c] as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    let mut out = Cursor::new(Vec::new());
    small
        .write_to(&mut out, ImageFormat::Png)
        .map_err(|e| Error::msg(format!("png: {e}")))?;
    Ok((out.into_inner(), w, h))
}

const LOGO_DIR: &str = "/System/Library/PrivateFrameworks/ProgressUI.framework";
const JOB: &str = "/var/root/Library/Caches/vphone-linux-remount-rw.plist";
const JOB_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>vphone-linux.remount-rw</string>
<key>ProgramArguments</key><array><string>/sbin/mount</string><string>-u</string><string>-w</string><string>/</string></array>
<key>RunAtLoad</key><true/><key>LaunchOnlyOnce</key><true/>
</dict></plist>
"#;

fn make_root_writable(host: &dyn Host) -> Result<()> {
    rpc(host, "files.write", json!({"path": JOB, "content": JOB_PLIST}))?;
    let loaded = rpc(host, "services.load", json!({"paths": [JOB]}));
    if loaded.is_ok() {
        std::thread::sleep(Duration::from_secs(10));
    }
    let _ = rpc(host, "services.unload", json!({"paths": [JOB], "force": true}));
    let _ = rpc(host, "files.remove", json!({"path": JOB}));
    loaded.map(|_| ())
}

/// `guest-1x-logo.py`: write a 1x boot logo beside each @2x one. Returns how many.
pub fn write_1x_logos(host: &dyn Host) -> Result<usize> {
    let names: Vec<String> = files_list(host, LOGO_DIR)?
        .iter()
        .filter_map(|e| e["name"].as_str().map(str::to_owned))
        .collect();
    let want: Vec<&String> = names
        .iter()
        .filter(|n| n.starts_with("apple-logo") && n.ends_with("@2x~iphone.png"))
        .filter(|n| !names.contains(&n.replace("@2x~iphone.png", "~iphone.png")))
        .collect();
    if want.is_empty() {
        return Ok(0);
    }
    make_root_writable(host)?;
    let mut done = 0;
    for name in want {
        let data = files_read(host, &format!("{LOGO_DIR}/{name}"))?;
        let (png, w, h) = half_size_png(&data)?;
        let out = name.replace("@2x~iphone.png", "~iphone.png");
        files_write(host, &format!("{LOGO_DIR}/{out}"), &png)?;
        host.log(&format!("{out}: {w}x{h} from {name}"));
        done += 1;
    }
    Ok(done)
}
