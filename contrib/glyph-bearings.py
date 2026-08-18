#!/usr/bin/env python3
"""Regenerate `src/glyph.rs` from the nerd font the bar draws with.

Every icon in the *Mono* nerd variant advances one text cell, but its ink can be
a fifth of that cell wide. The bar subtracts a glyph's own side bearings from the
gap it puts next to text, so the ink-to-text distance is the same in every
module; those bearings are what this script measures.

Run it after changing `theme::font` or adding a glyph constant:

    python3 contrib/glyph-bearings.py

Needs `fonttools` and the font installed. Only glyphs whose bearings round to
something visible are written out, so the table stays short.
"""

from __future__ import annotations

import pathlib
import re
import sys

from fontTools.pens.boundsPen import BoundsPen
from fontTools.ttLib import TTFont

#: Must match the family in `theme::font`.
FONT = "/usr/share/fonts/OTF/CommitMonoNerdFontMono-Regular.otf"
FAMILY = "CommitMono Nerd Font Mono"
#: Below this the bearing is under a fifth of a pixel at the bar's font size.
FLOOR = 0.002

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "src" / "glyph.rs"

HEADER = f'''//! Side bearings of the nerd-font glyphs the bar draws, in em.
//!
//! In the Mono nerd variant every icon advances one cell, but its ink can be
//! much narrower than that cell: a thermometer is a fifth as wide as a memory
//! chip. Laying a glyph next to text with one fixed gap therefore *looks* like a
//! different gap per module. Subtracting the glyph's own right bearing from that
//! gap makes the ink-to-text distance the same everywhere.
//!
//! Generated from `{FAMILY}` (the family in `theme::font`) by
//! `contrib/glyph-bearings.py`; rerun it after changing the font or a glyph
//! constant.

/// `(codepoint, left bearing, right bearing)`, sorted by codepoint. Glyphs whose
/// ink fills the cell are omitted: their bearings round to zero. The left
/// bearing is unused today and kept because it is the same measurement.
const BEARINGS: &[(u32, f32, f32)] = &[
'''

FOOTER = '''];

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
'''


def codepoints() -> set[int]:
    """Every `\\u{...}` escape in the bar's source: the glyph set it can draw."""
    found: set[int] = set()
    for path in sorted((ROOT / "src").rglob("*.rs")):
        for match in re.finditer(r"\\u\{([0-9a-fA-F]+)\}", path.read_text()):
            found.add(int(match.group(1), 16))
    return found


def main() -> int:
    font = TTFont(FONT)
    upem = font["head"].unitsPerEm
    cmap = font.getBestCmap()
    glyphs = font.getGlyphSet()

    rows = []
    for codepoint in sorted(codepoints()):
        name = cmap.get(codepoint)
        if name is None:
            continue
        bounds = BoundsPen(glyphs)
        glyphs[name].draw(bounds)
        if bounds.bounds is None:  # a space, or an unmapped icon
            continue
        advance = font["hmtx"][name][0]
        left = bounds.bounds[0] / upem
        right = (advance - bounds.bounds[2]) / upem
        if left < FLOOR and right < FLOOR:
            continue
        rows.append(f"    (0x{codepoint:05X}, {round(left, 4):.4}, {round(right, 4):.4}),\n")

    OUT.write_text(HEADER + "".join(rows) + FOOTER)
    print(f"{OUT.relative_to(ROOT)}: {len(rows)} glyphs with visible bearings")
    return 0


if __name__ == "__main__":
    sys.exit(main())
