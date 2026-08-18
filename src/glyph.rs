//! Side bearings of the nerd-font glyphs the bar draws, in em.
//!
//! In the Mono nerd variant every icon advances one cell, but its ink can be
//! much narrower than that cell: a thermometer is a fifth as wide as a memory
//! chip. Laying a glyph next to text with one fixed gap therefore *looks* like a
//! different gap per module. Subtracting the glyph's own right bearing from that
//! gap makes the ink-to-text distance the same everywhere.
//!
//! Generated from `CommitMono Nerd Font Mono` (the family in `theme::font`) by
//! `contrib/glyph-bearings.py`; rerun it after changing the font or a glyph
//! constant.

/// `(codepoint, left bearing, right bearing)`, sorted by codepoint. Glyphs whose
/// ink fills the cell are omitted: their bearings round to zero. The left
/// bearing is unused today and kept because it is the same measurement.
const BEARINGS: &[(u32, f32, f32)] = &[
    (0xF0079, 0.05, 0.05),
    (0xF007B, 0.05, 0.05),
    (0xF007C, 0.05, 0.05),
    (0xF007D, 0.05, 0.05),
    (0xF007E, 0.05, 0.05),
    (0xF007F, 0.05, 0.05),
    (0xF0080, 0.05, 0.05),
    (0xF0081, 0.05, 0.05),
    (0xF0082, 0.05, 0.05),
    (0xF008E, 0.05, 0.05),
    (0xF0091, 0.05, 0.05),
    (0xF00AF, 0.0355, 0.0355),
    (0xF00DC, 0.12, 0.09),
    (0xF011C, 0.0365, 0.0365),
    (0xF0140, 0.042, 0.043),
    (0xF0142, 0.153, 0.129),
    (0xF0210, 0.0505, 0.0495),
    (0xF0241, 0.091, 0.091),
    (0xF04C3, 0.009, 0.009),
    (0xF050F, 0.0913, 0.0913),
    (0xF057F, 0.0665, 0.0665),
    (0xF0589, 0.022, 0.022),
    (0xF10C2, 0.0913, 0.0913),
    (0xF10C3, 0.0913, 0.0913),
];

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
