//! Compares this crate's output byte for byte with the Python tools it ports,
//! running the tools themselves from this checkout.

mod common;
use common::*;
use std::fs;
use std::path::Path;
use std::process::Command;
use vphone_create_steps as vc;
use vc::display;
use vc::nvram::{self, NvramEdit};
use vc::plist_py;
use vc::progress::Silent;

/// Makes an nvram.bin with several banks, a truncated stale entry and a `common`
/// partition whose length overruns its bank, as observed on real guests.
const NVRAM_FIXTURE: &str = r#"
import struct, sys, zlib
def chk(h):
    t = h[0]
    for i in range(14):
        t += h[2 + i]; t = (t + ((t & 0xFF00) >> 8)) & 0xFF
    return t & 0xFF
def part(name, size16):
    h = bytearray(16); h[0] = 0x70; h[2:4] = struct.pack(">H", size16)
    h[4:4+len(name)] = name; return h
data = bytearray(33570816)
def bank(base, length, env, gen, common_len=None):
    bb = bytearray(length)
    h = bytearray(16); h[0] = 0x5A; h[2:4] = struct.pack(">H", length // 16); h[4:9] = b"nvram"
    h[1] = chk(bytes(h)); bb[0:16] = h
    p = part(b"common", common_len if common_len is not None else (length - 0x20) // 16)
    bb[0x20:0x30] = p
    blob = b"".join(e + b"\0" for e in env)
    bb[0x30:0x30+len(blob)] = blob
    struct.pack_into("<I", bb, 16 + 4, gen)
    struct.pack_into("<I", bb, 16, zlib.adler32(bytes(bb[0x14:])) & 0xFFFFFFFF)
    data[base:base+length] = bb
env = [b"auto-boot=false", b"boot-args=rd=md0 nand-enable-reformat=1 -progress", b"-enable-reformat=1 -progress", b"one-time-boot-command=device-recovery", b"device-recovery-boot-reason=foo", b"platform-uuid=x"]
bank(0x4000 + 0x80000, 0x80000, env, 3, 0xFE7F)
bank(0x4000 + 0x100000, 0x80000, env[:1] + env[3:], 7)
open(sys.argv[1], "wb").write(bytes(data))
"#;

fn nvram_case(args: &[&str], edit: NvramEdit) {
    let dir = tmp();
    let tool = dir.path().join("tool.bin");
    let ours = dir.path().join("ours.bin");
    python(NVRAM_FIXTURE, &[tool.to_str().unwrap()]);
    fs::copy(&tool, &ours).unwrap();
    let mut argv = vec![tool.to_str().unwrap()];
    argv.extend_from_slice(args);
    python_tool("tools/nvram-autoboot.py", &argv);
    nvram::set_file(&ours, &edit).unwrap();
    assert!(fs::read(&tool).unwrap() == fs::read(&ours).unwrap(), "nvram differs for {args:?}");
}

#[test]
fn nvram_matches_python() {
    nvram_case(&["--auto-boot", "true"], NvramEdit::auto_boot(true));
    nvram_case(&["--auto-boot", "false"], NvramEdit::auto_boot(false));
    nvram_case(
        &["--auto-boot", "true", "--drop", "one-time-boot-command", "--drop", "device-recovery-boot-reason"],
        NvramEdit {
            auto_boot: true,
            boot_args: None,
            drop: vec!["one-time-boot-command".into(), "device-recovery-boot-reason".into()],
        },
    );
    nvram_case(
        &["--auto-boot", "true", "--boot-args", "-v serial=3"],
        NvramEdit {
            auto_boot: true,
            boot_args: Some("-v serial=3".into()),
            drop: vec![],
        },
    );
}

#[test]
fn nvram_no_change_is_detected_and_no_banks_is_an_error() {
    let dir = tmp();
    let path = dir.path().join("n.bin");
    python(NVRAM_FIXTURE, &[path.to_str().unwrap()]);
    assert!(nvram::set_file(&path, &NvramEdit::auto_boot(true)).unwrap());
    assert!(!nvram::set_file(&path, &NvramEdit::auto_boot(true)).unwrap());
    let empty = dir.path().join("e.bin");
    fs::write(&empty, vec![0u8; 4096]).unwrap();
    assert!(nvram::set_file(&empty, &NvramEdit::auto_boot(true)).is_err());
}

fn llb_fixture(path: &Path) {
    // ubfx w8, w9, #16, #8 ; add w8, w8, #1 ; str w8, [x10, #0x10], at a 4-aligned offset.
    let mut d = vec![0u8; 0x8000];
    let rd = 8u32;
    let ubfx = 0x5300_0000 | (16 << 16) | (23 << 10) | (9 << 5) | rd;
    let add = 0x1100_0000 | (1 << 10) | (rd << 5) | rd;
    let strw = 0xB900_0000 | (4 << 10) | (10 << 5) | rd;
    for (i, insn) in [ubfx, add, strw].iter().enumerate() {
        d[0x4100 + 4 * i..0x4104 + 4 * i].copy_from_slice(&insn.to_le_bytes());
    }
    fs::write(path, d).unwrap();
}

#[test]
fn display_scale_matches_python() {
    for scale in [1u32, 2, 3, 0] {
        let dir = tmp();
        let tool = dir.path().join("tool.bin");
        let ours = dir.path().join("ours.bin");
        llb_fixture(&tool);
        fs::copy(&tool, &ours).unwrap();
        python_tool("tools/display-scale.py", &[tool.to_str().unwrap(), &scale.to_string()]);
        display::patch_file(&ours, scale).unwrap();
        assert!(fs::read(&tool).unwrap() == fs::read(&ours).unwrap(), "scale {scale}");
        let data = fs::read(&ours).unwrap();
        let state = display::current(&data).unwrap();
        if scale == 0 {
            assert_eq!(state, display::LlbScale::BootVideo);
        } else {
            assert_eq!(state, display::LlbScale::Patched(scale));
        }
    }
}

#[test]
fn display_scale_requires_exactly_one_site() {
    let dir = tmp();
    let p = dir.path().join("x.bin");
    llb_fixture(&p);
    let mut d = fs::read(&p).unwrap();
    let copy: Vec<u8> = d[0x4100..0x410C].to_vec();
    d[0x5000..0x500C].copy_from_slice(&copy);
    assert!(display::current(&d).is_err());
    assert!(display::current(&vec![0u8; 256]).is_err());
}

const PLISTLIB_CONFIG: &str = r#"
import plistlib, sys
out, ecid = sys.argv[1], int(sys.argv[2], 16)
plistlib.dump({
    "schemaVersion": 2, "platformType": "vresearch101", "cpuCount": 8, "memorySize": 8 * 2**30,
    "screenConfig": {"width": 1398, "height": 2034, "pixelsPerInch": 460, "scale": 3.0},
    "romImages": {"avpBooter": "OpenAVB.bin", "avpSEPBooter": "OpenSEP.bin"},
    "diskImage": "Disk.img", "nvramStorage": "nvram.bin", "sepStorage": "SEPStorage",
    "networkConfig": {"mode": "nat", "macAddress": "02:11:22:33:44:55"},
    "guestProductType": "iPhone19,4",
    "machineIdentifier": plistlib.dumps({"ECID": ecid}, fmt=plistlib.FMT_BINARY),
}, open(out, "wb"))
"#;

#[test]
fn config_plist_matches_plistlib() {
    let dir = tmp();
    let py = dir.path().join("py.plist");
    let ecid = 0x0000_4000_ABCD_1234u64;
    python(PLISTLIB_CONFIG, &[py.to_str().unwrap(), &format!("{ecid:x}")]);
    let screen = vc::bundle::Screen { width: 1398, height: 2034, ppi: 460, scale: 3.0 };
    let avb = dir.path().join("a.bin");
    let params = vc::bundle::BundleParams {
        vm: dir.path(),
        device: Some("iPhone19,4"),
        screen,
        scale: 1,
        width_policy: Default::default(),
        ecid: Some(ecid),
        memory_gib: 8,
        cpus: 8,
        open_avb: &avb,
        open_sep: &avb,
    };
    let ours = vc::bundle::config_plist(&params, ecid, "02:11:22:33:44:55");
    assert_eq!(String::from_utf8(ours).unwrap(), fs::read_to_string(&py).unwrap());
}

#[test]
fn machine_identifier_matches_plistlib_at_every_int_width() {
    for ecid in [1u64, 200, 0x1234, 0xFF_FFFF, 0x1_0000_0000, 0x4000_ABCD_1234, 0x7FFF_FFFF_FFFF_FFFF, 0x8000_0000_0000_0000] {
        let py = python(
            "import plistlib,sys;print(plistlib.dumps({'ECID':int(sys.argv[1],16)},fmt=plistlib.FMT_BINARY).hex())",
            &[&format!("{ecid:x}")],
        );
        assert_eq!(py.trim(), hex::encode(plist_py::machine_identifier(ecid)), "ecid {ecid:x}");
    }
}

#[test]
fn xml_writer_matches_plistlib_on_awkward_values() {
    let program = r#"
import plistlib, sys
v = {"z": [True, False, 1, -5, 2**40, 1.5, 3.0, "a<&>b", "", b"", b"x"*200, [], {}],
     "a": {"nested": {"deep": {"data": b"0123456789"*30, "list": [b"yy"*100]}}}}
sys.stdout.write(plistlib.dumps(v, fmt=plistlib.FMT_XML).decode())
"#;
    let py = python(program, &[]);
    let v: plist::Value = plist::from_bytes(py.as_bytes()).unwrap();
    assert_eq!(String::from_utf8(plist_py::to_xml(&v)).unwrap(), py);
}

/// The Python heredoc of tools/restore-tree-mode.sh that follows `marker`.
fn heredoc_after(script: &str, marker: &str) -> String {
    let text = fs::read_to_string(repo().join(script)).unwrap();
    let start = text.find(marker).expect("marker") + marker.len();
    let rest = &text[start..];
    let begin = rest.find("<<'PY'\n").expect("heredoc") + "<<'PY'\n".len();
    let end = rest[begin..].find("\nPY\n").expect("end");
    rest[begin..begin + end + 1].to_owned()
}

fn im4p(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    for s in [b"IM4P".as_slice(), tag.as_slice(), b"desc".as_slice()] {
        body.push(0x16);
        vc::der::encode_len(s.len(), &mut body);
        body.extend_from_slice(s);
    }
    body.extend_from_slice(&vc::der::encode(0x04, payload));
    vc::der::encode(0x30, &body)
}

fn manifest_fixture(tree: &Path) -> plist::Value {
    fs::create_dir_all(tree.join("Firmware")).unwrap();
    fs::write(tree.join("kernelcache.research.vphone600"), im4p(b"krnl", b"kernel bytes")).unwrap();
    fs::write(tree.join("kernelcache.release"), im4p(b"krnl", b"release kernel")).unwrap();
    fs::write(tree.join("ramdisk.dmg"), im4p(b"rdsk", b"ramdisk")).unwrap();
    fs::write(tree.join("Firmware/rd.trustcache"), im4p(b"trst", b"tc")).unwrap();
    fs::write(tree.join("Firmware/txm.im4p"), im4p(b"trxm", b"txm")).unwrap();
    let comp = |path: &str| {
        let mut info = plist::Dictionary::new();
        info.insert("Path".into(), plist::Value::from(path));
        let mut d = plist::Dictionary::new();
        d.insert("Info".into(), plist::Value::Dictionary(info));
        d.insert("Digest".into(), plist::Value::Data(vec![1, 2, 3]));
        plist::Value::Dictionary(d)
    };
    let mut man = plist::Dictionary::new();
    man.insert("RestoreKernelCache".into(), comp("kernelcache.release"));
    man.insert("KernelCache".into(), comp("kernelcache.research.vphone600"));
    man.insert("RestoreRamDisk".into(), comp("ramdisk.dmg"));
    man.insert("RestoreTrustCache".into(), comp("Firmware/rd.trustcache"));
    man.insert("Ap,RestoreTrustedExecutionMonitor".into(), comp("Firmware/txm.im4p"));
    let mut identity = plist::Dictionary::new();
    identity.insert("Manifest".into(), plist::Value::Dictionary(man));
    let mut root = plist::Dictionary::new();
    root.insert("BuildIdentities".into(), plist::Value::Array(vec![plist::Value::Dictionary(identity)]));
    let v = plist::Value::Dictionary(root);
    fs::write(tree.join("BuildManifest.plist"), plist_py::to_xml(&v)).unwrap();
    v
}

#[test]
fn restore_tree_manifest_matches_the_shell_heredoc() {
    let dir = tmp();
    let tree = dir.path();
    let mut v = manifest_fixture(tree);
    let manifest = tree.join("BuildManifest.plist");
    let copy = tree.join("py.plist");
    fs::copy(&manifest, &copy).unwrap();
    let code = heredoc_after("tools/restore-tree-mode.sh", "restore kernel ->");
    python(&code, &[copy.to_str().unwrap(), tree.to_str().unwrap(), "kernelcache.research.vphone600"]);
    let ours = vc::manifest::apply_ssh_digests(&mut v, tree, "kernelcache.research.vphone600").unwrap();
    assert_eq!(String::from_utf8(ours).unwrap(), fs::read_to_string(&copy).unwrap());
}

#[test]
fn admit_matches_the_shell_heredoc() {
    let dir = tmp();
    let mut root = plist::Dictionary::new();
    root.insert("SupportedProductTypes".into(), plist::Value::Array(vec!["iPhone1,1".into()]));
    let py = dir.path().join("py.plist");
    let ours = dir.path().join("ours.plist");
    let bytes = plist_py::to_xml(&plist::Value::Dictionary(root));
    fs::write(&py, &bytes).unwrap();
    fs::write(&ours, &bytes).unwrap();
    let code = heredoc_after("tools/gpu-driver-from-cloudos.sh", "unzip -q");
    python(&code, &[py.to_str().unwrap()]);
    vc::steps::admit_temporary_vm(&ours).unwrap();
    assert_eq!(fs::read(&py).unwrap(), fs::read(&ours).unwrap());
}

#[test]
fn trustcache_matches_python_on_the_real_tools() {
    let tree = sshtars();
    assert!(tree.join("usr/local/bin/dropbear").exists(), "run sshtars fetch first");
    let dir = tmp();
    let mut payload = vec![1, 0, 0, 0];
    payload.extend_from_slice(&[0xAB; 16]);
    payload.extend_from_slice(&2u32.to_le_bytes());
    for (b, flag) in [(0x11u8, [1u8, 0]), (0xEE, [2, 0])] {
        payload.extend_from_slice(&[b; 20]);
        payload.extend_from_slice(&flag);
    }
    let stock = dir.path().join("stock.rtsc");
    fs::write(&stock, im4p(b"rtsc", &payload)).unwrap();
    let py = dir.path().join("py.rtsc");
    let ours = dir.path().join("ours.rtsc");
    let out = python_tool(
        "tools/ssh-ramdisk-trustcache.py",
        &[stock.to_str().unwrap(), tree.to_str().unwrap(), py.to_str().unwrap()],
    );
    println!("{out}");
    let built = vc::trustcache::build(&fs::read(&stock).unwrap(), &tree, &ours, &Silent).unwrap();
    println!("{built:?}");
    assert!(built.added > 100, "expected the staging tree's Mach-Os to be added");
    assert!(fs::read(&py).unwrap() == fs::read(&ours).unwrap(), "trust caches differ");
}

#[test]
fn rdsk_repack_matches_the_shell_tool() {
    let dir = tmp();
    let plen = 4096usize;
    let mut payload = vec![7u8; plen];
    payload[32..36].copy_from_slice(b"NXSB");
    let mut stock = vec![0x30, 0x84];
    stock.extend_from_slice(&((plen + 21) as u32).to_be_bytes());
    stock.extend_from_slice(b"\x16\x04IM4P\x16\x04rdsk\x16\x010\x04\x84");
    stock.extend_from_slice(&(plen as u32).to_be_bytes());
    stock.extend_from_slice(&vec![9u8; plen]);
    let stock_path = dir.path().join("stock.dmg");
    fs::write(&stock_path, &stock).unwrap();
    let new_payload = dir.path().join("payload");
    fs::write(&new_payload, &payload).unwrap();
    let py = dir.path().join("py.dmg");
    let ours = dir.path().join("ours.dmg");
    let status = Command::new("bash")
        .arg(repo().join("tools/ssh-ramdisk-repack.sh"))
        .args([&stock_path, &new_payload, &py])
        .output()
        .unwrap();
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
    vc::im4p::repack_rdsk(&stock_path, &new_payload, &ours).unwrap();
    assert_eq!(fs::read(&py).unwrap(), fs::read(&ours).unwrap());
    // And the payload comes back out of the stock wrapper.
    let extracted = dir.path().join("x");
    vc::im4p::extract_rdsk_payload(&stock_path, &extracted).unwrap();
    assert_eq!(fs::read(&extracted).unwrap(), vec![9u8; plen]);
    // A changed length is refused.
    fs::write(&new_payload, vec![0u8; plen + 1]).unwrap();
    assert!(vc::im4p::repack_rdsk(&stock_path, &new_payload, &ours).is_err());
}

#[test]
fn launchd_injection_matches_plistlib() {
    let daemon = guest_resources().join("vphoned.plist");
    let dir = tmp();
    let launchd = dir.path().join("launchd.plist");
    let program = r#"
import plistlib, sys
base = {"LaunchDaemons": {"/System/Library/LaunchDaemons/a.plist": {"Label": "a"}}, "Other": 1}
open(sys.argv[1], "wb").write(plistlib.dumps(base, fmt=plistlib.FMT_BINARY))
m = plistlib.load(open(sys.argv[1], "rb"))
m["LaunchDaemons"]["/System/Library/LaunchDaemons/vphoned.plist"] = plistlib.load(open(sys.argv[2], "rb"))
sys.stdout.write(plistlib.dumps(m, fmt=plistlib.FMT_XML).decode())
"#;
    let expected = python(program, &[launchd.to_str().unwrap(), daemon.to_str().unwrap()]);
    let ours = vc::cfw::inject_daemon(&fs::read(&launchd).unwrap(), "vphoned", &fs::read(&daemon).unwrap()).unwrap();
    assert_eq!(String::from_utf8(ours).unwrap(), expected);
}

fn tool_functions(script: &str, from: &str, to: &str) -> String {
    let text = fs::read_to_string(repo().join(script)).unwrap();
    let a = text.find(from).expect("from");
    let b = text.find(to).expect("to");
    text[a..b].to_owned()
}

#[test]
fn dt_scale_matches_guest_dt_scale_py() {
    // A device tree: root {name} / product {name, display-scale, artwork-scale-factor} / display0 {display-scale}
    fn node(name: &str, props: &[(&str, Vec<u8>)], children: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&((props.len() + 1) as u32).to_le_bytes());
        out.extend_from_slice(&(children.len() as u32).to_le_bytes());
        let mut all = vec![("name", {
            let mut v = name.as_bytes().to_vec();
            v.push(0);
            v
        })];
        all.extend(props.iter().cloned());
        for (k, v) in all {
            let mut key = [0u8; 32];
            key[..k.len()].copy_from_slice(k.as_bytes());
            out.extend_from_slice(&key);
            out.extend_from_slice(&(v.len() as u32).to_le_bytes());
            out.extend_from_slice(&v);
            out.resize(out.len() + ((4 - v.len() % 4) % 4), 0);
        }
        for c in children {
            out.extend_from_slice(c);
        }
        out
    }
    let three = 3u32.to_le_bytes().to_vec();
    let display0 = node("display0", &[("display-scale", three.clone())], &[]);
    let product = node("product", &[("artwork-scale-factor", three.clone()), ("other", vec![9, 9])], &[display0]);
    let outside = node("chosen", &[("display-scale", three.clone())], &[]);
    let root = node("device-tree", &[], &[product, outside]);
    let mut img4_body = Vec::new();
    for s in [b"IMG4".as_slice()] {
        img4_body.extend_from_slice(&vc::der::encode(0x16, s));
    }
    let mut im4p_body = Vec::new();
    for s in [b"IM4P".as_slice(), b"dtre", b"d"] {
        im4p_body.extend_from_slice(&vc::der::encode(0x16, s));
    }
    im4p_body.extend_from_slice(&vc::der::encode(0x04, &root));
    img4_body.extend_from_slice(&vc::der::encode(0x30, &im4p_body));
    let mut img4 = vc::der::encode(0x30, &img4_body);
    let dir = tmp();
    let path = dir.path().join("dt.img4");
    fs::write(&path, &img4).unwrap();
    let functions = tool_functions("tools/guest-dt-scale.py", "def der(buf, off):", "boot = [e[\"name\"]");
    let program = format!(
        "import struct, sys\n{functions}\nimg4 = bytearray(open(sys.argv[1],'rb').read())\nstart, end = payload(img4)\nhits = []\nwalk(img4, start, '', hits)\nfor _, at in hits:\n    struct.pack_into('<I', img4, at, 1)\nopen(sys.argv[1],'wb').write(img4)\nprint(len(hits))\n"
    );
    let n = python(&program, &[path.to_str().unwrap()]);
    let count = vc::guest::set_dt_scale(&mut img4, 1).unwrap();
    assert_eq!(n.trim().parse::<usize>().unwrap(), count);
    assert_eq!(count, 2, "the /chosen display-scale must be left alone");
    assert_eq!(fs::read(&path).unwrap(), img4);
}

#[test]
fn cgbi_decode_matches_guest_1x_logo_py() {
    let dir = tmp();
    let path = dir.path().join("logo.png");
    let out = dir.path().join("rgba.bin");
    // Hand-built CgBI: 2x2, BGRA premultiplied, raw deflate, filters 0 and 2.
    let make = r#"
import struct, sys, zlib
def chunk(t, b):
    return struct.pack(">I", len(b)) + t + b + struct.pack(">I", zlib.crc32(t + b) & 0xffffffff)
w, h = 3, 2
raw = bytes([0]) + bytes([10, 20, 30, 128, 0, 0, 0, 0, 255, 128, 64, 255])
raw += bytes([2]) + bytes([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12])
co = zlib.compressobj(9, zlib.DEFLATED, -15)
idat = co.compress(raw) + co.flush()
d = b"\x89PNG\r\n\x1a\n" + chunk(b"CgBI", b"\0\0\0\0") + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)) + chunk(b"IDAT", idat) + chunk(b"IEND", b"")
open(sys.argv[1], "wb").write(d)
"#;
    python(make, &[path.to_str().unwrap()]);
    let functions = tool_functions("tools/guest-1x-logo.py", "def decode_png(data):", "JOB = ");
    let program = format!(
        "import io, struct, sys, zlib\nfrom PIL import Image\n{functions}\nim = decode_png(open(sys.argv[1],'rb').read())\nopen(sys.argv[2],'wb').write(im.tobytes())\nprint(im.mode, im.size)\n"
    );
    println!("{}", Command::new("python3").arg("-c").arg(&program).args([path.to_str().unwrap(), out.to_str().unwrap()]).status().unwrap());
    let ours = vc::guest::decode_png(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(ours.as_raw(), &fs::read(&out).unwrap());
}

#[test]
fn half_size_png_is_close_to_pillow_lanczos() {
    let dir = tmp();
    let src = dir.path().join("src.png");
    let py = dir.path().join("py.bin");
    let program = r#"
import sys
from PIL import Image, ImageDraw
im = Image.new("RGBA", (200, 120), (0, 0, 0, 0))
d = ImageDraw.Draw(im)
d.ellipse((30, 10, 170, 110), fill=(255, 255, 255, 255))
im.save(sys.argv[1])
s = im.resize((100, 60), Image.LANCZOS)
open(sys.argv[2], "wb").write(s.tobytes())
"#;
    assert!(Command::new("python3").arg("-c").arg(program).args([src.to_str().unwrap(), py.to_str().unwrap()]).status().unwrap().success());
    let (png, w, h) = vc::guest::half_size_png(&fs::read(&src).unwrap()).unwrap();
    assert_eq!((w, h), (100, 60));
    let ours = image::load_from_memory(&png).unwrap().to_rgba8();
    let theirs = fs::read(&py).unwrap();
    // Colour under almost-zero alpha is invisible; compare what is displayed.
    let premult = |p: &[u8]| -> [i32; 4] {
        let a = p[3] as i32;
        [p[0] as i32 * a / 255, p[1] as i32 * a / 255, p[2] as i32 * a / 255, a]
    };
    let mut max = 0i32;
    let mut sum = 0u64;
    for (a, b) in ours.as_raw().chunks_exact(4).zip(theirs.chunks_exact(4)) {
        let (a, b) = (premult(a), premult(b));
        for i in 0..4 {
            let d = (a[i] - b[i]).abs();
            max = max.max(d);
            sum += d as u64;
        }
    }
    let mean = sum as f64 / theirs.len() as f64;
    println!("max per-channel difference {max}, mean {mean:.3}");
    assert!(mean < 2.0 && max <= 32, "max {max} mean {mean}");
}
