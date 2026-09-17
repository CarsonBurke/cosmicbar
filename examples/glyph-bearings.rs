//! Regenerate `src/glyph.rs` from the font used by `theme::icon_font`.
//!
//! Run `cargo run --example glyph-bearings` after changing the font or adding a
//! glyph constant. The font must be installed at FONT; no font tooling is needed.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use ttf_parser::{Face, OutlineBuilder, PlatformId};

const FONT: &str = "/usr/share/fonts/OTF/CommitMonoNerdFontMono-Regular.otf";
const FLOOR: f64 = 0.002;

const HEADER: &str = "//! Side bearings of the nerd-font glyphs the bar draws, in em.
//!
//! In the Mono nerd variant every icon advances one cell, but its ink can be
//! much narrower than that cell: a thermometer is a fifth as wide as a memory
//! chip. Laying a glyph next to text with one fixed gap therefore *looks* like a
//! different gap per module. Subtracting the glyph's own right bearing from that
//! gap makes the ink-to-text distance the same everywhere.
//!
//! Generated from `CommitMono Nerd Font Mono` (the family in `theme::icon_font`)
//! by `cargo run --example glyph-bearings`; rerun it after changing the icon font
//! or a glyph constant.

/// `(codepoint, left bearing, right bearing)`, sorted by codepoint. Glyphs whose
/// ink fills the cell are omitted: their bearings round to zero. The left
/// bearing is unused today and kept because it is the same measurement.
const BEARINGS: &[(u32, f32, f32)] = &[
";

const FOOTER: &str = "];

/// Space between a glyph's ink and the right edge of its cell, in em. `0.0` for
/// text and for icons that fill their cell.
pub fn right_bearing(glyph: &str) -> f32 {
    let Some(last) = glyph.chars().next_back() else {
        return 0.0;
    };
    lookup(last).map_or(0.0, |(_, rsb)| rsb)
}

fn lookup(c: char) -> Option<(f32, f32)> {
    BEARINGS
        .binary_search_by_key(&(c as u32), |(cp, _, _)| *cp)
        .ok()
        .map(|index| {
            let (_, lsb, rsb) = BEARINGS[index];
            (lsb, rsb)
        })
}
";

fn codepoints(dir: &Path, found: &mut BTreeSet<u32>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            codepoints(&path, found)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let source = fs::read_to_string(&path)?;
            for suffix in source.split("\\u{").skip(1) {
                let Some((hex, _)) = suffix.split_once('}') else {
                    continue;
                };
                if !hex.is_empty() && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    found.insert(u32::from_str_radix(hex, 16)?);
                }
            }
        }
    }
    Ok(())
}

// Font bounding boxes are integer-rounded (and control-point boxes can be too
// wide). Measure actual Bezier extrema, like BoundsPen, before normalizing to em.
// Only horizontal bounds affect side bearings; move-only contours have no ink.
#[derive(Default)]
struct InkBounds {
    current: f64,
    bounds: Option<(f64, f64)>,
}

impl InkBounds {
    fn include(&mut self, x: f64) {
        self.bounds = Some(match self.bounds {
            Some((min, max)) => (min.min(x), max.max(x)),
            None => (x, x),
        });
    }
}

impl OutlineBuilder for InkBounds {
    fn move_to(&mut self, x: f32, _: f32) {
        self.current = f64::from(x);
    }

    fn line_to(&mut self, x: f32, _: f32) {
        self.include(self.current);
        self.current = f64::from(x);
        self.include(self.current);
    }

    fn quad_to(&mut self, x1: f32, _: f32, x: f32, _: f32) {
        let (p0, p1, p2) = (self.current, f64::from(x1), f64::from(x));
        self.include(p0);
        self.include(p2);
        let denominator = p0 - 2.0 * p1 + p2;
        if denominator != 0.0 {
            let t = (p0 - p1) / denominator;
            if t > 0.0 && t < 1.0 {
                let s = 1.0 - t;
                self.include(s * s * p0 + 2.0 * s * t * p1 + t * t * p2);
            }
        }
        self.current = p2;
    }

    fn curve_to(&mut self, x1: f32, _: f32, x2: f32, _: f32, x: f32, _: f32) {
        let (p0, p1, p2, p3) = (self.current, f64::from(x1), f64::from(x2), f64::from(x));
        self.include(p0);
        self.include(p3);
        // The derivative divided by three is a*t^2 + b*t + c.
        let a = -p0 + 3.0 * p1 - 3.0 * p2 + p3;
        let b = 2.0 * (p0 - 2.0 * p1 + p2);
        let c = p1 - p0;
        let mut extrema = [f64::NAN; 2];
        if a == 0.0 {
            if b != 0.0 {
                extrema[0] = -c / b;
            }
        } else {
            let discriminant = b * b - 4.0 * a * c;
            if discriminant >= 0.0 {
                let root = discriminant.sqrt();
                extrema = [(-b + root) / (2.0 * a), (-b - root) / (2.0 * a)];
            }
        }
        for t in extrema {
            if t > 0.0 && t < 1.0 {
                let s = 1.0 - t;
                self.include(
                    s * s * s * p0 + 3.0 * s * s * t * p1 + 3.0 * s * t * t * p2 + t * t * t * p3,
                );
            }
        }
        self.current = p3;
    }

    fn close(&mut self) {}
}

fn main() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let data = fs::read(FONT).with_context(|| format!("reading {FONT}"))?;
    let font = Face::parse(&data, 0).context("parsing icon font")?;
    let upem = f64::from(font.units_per_em());
    let cmap = font
        .tables()
        .cmap
        .context("icon font has no character map")?;
    // Match fontTools getBestCmap: prefer Windows full Unicode, then Unicode
    // full repertoire, then BMP encodings. Do not mix mappings across subtables.
    let preferred = [
        (3, 10),
        (0, 6),
        (0, 4),
        (3, 1),
        (0, 3),
        (0, 2),
        (0, 1),
        (0, 0),
    ];
    let cmap = preferred
        .into_iter()
        .find_map(|(platform, encoding)| {
            cmap.subtables.into_iter().find(|table| {
                table.platform_id
                    == if platform == 3 {
                        PlatformId::Windows
                    } else {
                        PlatformId::Unicode
                    }
                    && table.encoding_id == encoding
            })
        })
        .context("icon font has no Unicode character map")?;
    let mut found = BTreeSet::new();
    codepoints(&root.join("src"), &mut found)?;
    let mut output = String::from(HEADER);
    let mut rows = 0;
    for codepoint in found {
        let Some(glyph) = cmap.glyph_index(codepoint) else {
            continue;
        };
        let mut bounds = InkBounds::default();
        if font.outline_glyph(glyph, &mut bounds).is_none() {
            continue;
        }
        let Some((min, max)) = bounds.bounds else {
            continue;
        };
        let advance = font
            .glyph_hor_advance(glyph)
            .context("glyph has no advance")?;
        let left = min / upem;
        let right = (f64::from(advance) - max) / upem;
        if left < FLOOR && right < FLOOR {
            continue;
        }
        writeln!(output, "    (0x{codepoint:05X}, {left:.4}, {right:.4}),")?;
        rows += 1;
    }
    output.push_str(FOOTER);
    fs::write(root.join("src/glyph.rs"), output)?;
    println!("src/glyph.rs: {rows} glyphs with visible bearings");
    Ok(())
}
