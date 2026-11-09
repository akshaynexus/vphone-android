//! Display scale. Ports tools/display-scale.py (the LLB patch in `nvram.bin`)
//! and the `gfx-res` arithmetic of tools/set-scale.sh and setup/lib/make-bundle.py.

use crate::bail;
use crate::error::{Error, Result};
use std::fs;
use std::path::Path;

/// What LLB's display-scale computation is doing now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LlbScale {
    /// The original `add` instruction (3x from the boot video).
    BootVideo,
    /// Patched to `mov wN, #scale`.
    Patched(u32),
}

struct Hit {
    off: usize,
    rd: u32,
    insn: u32,
    add: u32,
}

fn find(data: &[u8]) -> Result<Hit> {
    const PATTERN: u32 = 0x5300_0000 | (16 << 16) | (23 << 10);
    let mut hits = Vec::new();
    let mut off = 0;
    while off + 12 < data.len() {
        let a = u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
        off += 4;
        if a & 0xFFFF_FC00 != PATTERN {
            continue;
        }
        let at = off - 4;
        let b = u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
        let c = u32::from_le_bytes(data[off + 4..off + 8].try_into().unwrap());
        let rd = a & 31;
        let add = 0x1100_0000 | (1 << 10) | (rd << 5) | rd;
        let is_mov = b & 0xFFE0_001F == 0x5280_0000 | rd;
        let is_str = c & 0xFFC0_001F == 0xB900_0000 | rd || c & 0xFFE0_0C1F == 0xB800_0000 | rd;
        if is_str && (b == add || is_mov) {
            hits.push(Hit { off: at, rd, insn: b, add });
        }
    }
    match hits.len() {
        1 => Ok(hits.pop().unwrap()),
        n => Err(Error::msg(format!("expected one display-scale computation, found {n}"))),
    }
}

/// The current LLB display-scale state.
pub fn current(data: &[u8]) -> Result<LlbScale> {
    let hit = find(data)?;
    Ok(if hit.insn == hit.add {
        LlbScale::BootVideo
    } else {
        LlbScale::Patched((hit.insn >> 5) & 0xFFFF)
    })
}

/// `display-scale.py NVRAM SCALE`: SCALE 0 restores the original instruction.
pub fn patch(data: &mut [u8], scale: u32) -> Result<()> {
    if scale > 3 {
        bail!("display scale must be 0-3, not {scale}");
    }
    let hit = find(data)?;
    let new = if scale == 0 {
        hit.add
    } else {
        0x5280_0000 | (scale << 5) | hit.rd
    };
    data[hit.off + 4..hit.off + 8].copy_from_slice(&new.to_le_bytes());
    Ok(())
}

/// `patch` on a file, only rewriting it when a change was needed.
pub fn patch_file(nvram: &Path, scale: u32) -> Result<()> {
    let mut data = fs::read(nvram).map_err(|e| Error::path(nvram, e))?;
    let before = data.clone();
    patch(&mut data, scale)?;
    if data != before {
        fs::write(nvram, &data).map_err(|e| Error::path(nvram, e))?;
    }
    Ok(())
}

/// How a 1x point width is chosen for `gfx-res` below 3x.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WidthPolicy {
    /// The nearest multiple of 16 points, so `bytesPerRow` is a multiple of 64
    /// at 4 bytes per pixel and Adreno aliases guest surfaces in place
    /// (metal2vulkan's Adreno note; 430 points gives 432). Costs a few points of
    /// width. iOS keeps a device-specific layout only for exact points.
    #[default]
    AlignTo16,
    /// The panel's exact points (`native / native_scale`), what the desktop tools do.
    ExactPoints,
}

/// The panel size the scales are derived from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Native {
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

impl Native {
    pub fn points(self) -> (u32, u32) {
        (
            (self.width as f64 / self.scale).round() as u32,
            (self.height as f64 / self.scale).round() as u32,
        )
    }

    /// The width in points used at scales below 3x under `policy`.
    pub fn width_points(self, policy: WidthPolicy) -> u32 {
        let (w, _) = self.points();
        match policy {
            WidthPolicy::ExactPoints => w,
            WidthPolicy::AlignTo16 => {
                let down = w / 16 * 16;
                if w - down < 8 && down > 0 {
                    down
                } else {
                    down + 16
                }
            }
        }
    }

    /// The `gfx-res` for a UI `scale` (1, 2 or the panel's 3).
    pub fn render_size(self, scale: u32, policy: WidthPolicy) -> Result<(u32, u32)> {
        match scale {
            1 | 2 => {
                let (_, h) = self.points();
                Ok((self.width_points(policy) * scale, h * scale))
            }
            3 => Ok((self.width, self.height)),
            _ => Err(Error::msg(format!("scale must be 1, 2 or 3, not {scale}"))),
        }
    }
}

/// A `"WxH"` size file (gfx-res, gfx-res.3x).
pub fn read_res(path: &Path) -> Result<(u32, u32)> {
    let text = fs::read_to_string(path).map_err(|e| Error::path(path, e))?;
    let (w, h) = text
        .trim()
        .split_once('x')
        .ok_or_else(|| Error::msg(format!("{}: not WxH", path.display())))?;
    let parse = |s: &str| {
        s.trim()
            .parse::<u32>()
            .map_err(|_| Error::msg(format!("{}: not WxH", path.display())))
    };
    Ok((parse(w)?, parse(h)?))
}

pub fn write_res(path: &Path, (w, h): (u32, u32)) -> Result<()> {
    fs::write(path, format!("{w}x{h}\n")).map_err(|e| Error::path(path, e))
}
