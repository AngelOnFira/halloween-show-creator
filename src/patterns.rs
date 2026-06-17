//! The fixed laser pattern library, decoded once from the legacy
//! `patterns.json`. Lasers "pick from" these 38 shapes; the geometry is shared
//! by the timeline thumbnails, the right-click editor preview, and the 3D
//! viewport so they all match the already-seeded shows.
//!
//! A pattern is a list of galvo points (x/y in ~0..=300, r/g/b 3-bit 0..=7).
//! The legacy on-disk form packs each point into a 32-bit word; see `decode_u32`.

use std::collections::HashMap;
use std::sync::OnceLock;

use bevy_egui::egui;

/// One decoded pattern point. Galvo space `x`/`y` ~0..=300; `r`/`g`/`b` 3-bit.
#[derive(Clone, Copy)]
pub struct PatternPoint {
    pub x: i16,
    pub y: i16,
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// A named shape from the library.
pub struct Pattern {
    pub id: u8,
    pub name: &'static str,
    /// Non-blank points, in draw order.
    pub points: Vec<PatternPoint>,
}

/// Legacy galvo coordinate range, used to normalize for rendering.
pub const GALVO_MAX: f32 = 300.0;

/// Canonical ordered names. MUST stay in sync with `LASER_PATTERNS` in
/// `tools/show-seeder/src/main.rs` (and the legacy `value_lookup`), because
/// `LaserKeyframe.pattern` is an index into this list.
pub const PATTERN_NAMES: &[&str] = &[
    "bat",
    "bow",
    "bow_slow",
    "candy",
    "circle",
    "circle_slow",
    "clockwise_spiral_slow",
    "counterclockwise_spiral_slow",
    "crescent",
    "ghost",
    "gravestone_cross",
    "hexagon",
    "hexagon_slow",
    "horizontal_lines_left_to_right_slow",
    "horizontal_lines_right_to_left_slow",
    "lightning_bolt",
    "octagon",
    "octagon_slow",
    "parallelogram",
    "parallelogram_slow",
    "pentagon",
    "pentagon_slow",
    "pentagram",
    "pentagram_slow",
    "pumpkin",
    "septagon_slow",
    "square_large",
    "square_large_slow",
    "square_small",
    "square_small_slow",
    "star",
    "star_slow",
    "triangle_large",
    "triangle_large_slow",
    "triangle_small",
    "triangle_small_slow",
    "vertical_lines_bottom_to_top_slow",
    "vertical_lines_top_to_bottom_slow",
];

const PATTERNS_JSON: &str = include_str!("../assets/patterns.json");

/// Decode one packed 32-bit galvo point (msb0); `None` for a blank (`0x0`) point.
pub fn decode_u32(v: u32) -> Option<PatternPoint> {
    if v == 0 {
        return None;
    }
    Some(PatternPoint {
        x: ((v & 0xFF80_0000) >> 23) as i16,
        y: ((v & 0x007F_C000) >> 14) as i16,
        r: ((v & 0x0000_3800) >> 11) as u8,
        g: ((v & 0x0000_0700) >> 8) as u8,
        b: ((v & 0x0000_00E0) >> 5) as u8,
    })
}

fn build() -> Vec<Pattern> {
    let map: HashMap<String, Vec<String>> =
        serde_json::from_str(PATTERNS_JSON).expect("assets/patterns.json should parse");
    PATTERN_NAMES
        .iter()
        .enumerate()
        .map(|(id, &name)| {
            let points = map
                .get(name)
                .map(|hexes| {
                    hexes
                        .iter()
                        .filter_map(|h| {
                            let v = u32::from_str_radix(h.trim_start_matches("0x"), 16).ok()?;
                            decode_u32(v)
                        })
                        .collect()
                })
                .unwrap_or_default();
            Pattern {
                id: id as u8,
                name,
                points,
            }
        })
        .collect()
}

/// The full pattern library, built once on first use.
pub fn library() -> &'static [Pattern] {
    static LIB: OnceLock<Vec<Pattern>> = OnceLock::new();
    LIB.get_or_init(build)
}

/// Look up a pattern by its id (index into `PATTERN_NAMES`).
pub fn get(id: u8) -> Option<&'static Pattern> {
    library().get(id as usize)
}

/// Resolve a (possibly hyphenated) pattern name to its id.
#[allow(dead_code)] // public helper; currently only exercised by tests
pub fn name_to_id(name: &str) -> Option<u8> {
    let norm = name.replace('-', "_");
    PATTERN_NAMES.iter().position(|&n| n == norm).map(|i| i as u8)
}

/// Expand a 3-bit (0..=7) colour channel to 0..=255.
fn chan(c: u8) -> u8 {
    ((c.min(7) as f32 / 7.0) * 255.0).round() as u8
}

/// Is this point a beam blank (the laser is off here)? A zero RGB means no beam.
pub fn is_blank(p: &PatternPoint) -> bool {
    p.r == 0 && p.g == 0 && p.b == 0
}

/// The lit segments of a pattern as `(from, to)` point pairs, ready to draw.
///
/// The galvo traces a *closed loop* through the points (the last point returns
/// to the first), lighting each segment with its **source** point's colour. A
/// segment whose source is blank is a pen-up move and is omitted, so blanked
/// travel (e.g. between the bars of a shape, or back to home) draws nothing.
/// Exact `(0,0)` blank words are home/padding and are dropped entirely.
pub fn outline_segments(points: &[PatternPoint]) -> Vec<(PatternPoint, PatternPoint)> {
    let pts: Vec<PatternPoint> = points
        .iter()
        .copied()
        .filter(|p| !(p.x == 0 && p.y == 0 && is_blank(p)))
        .collect();
    if pts.len() < 2 {
        return Vec::new();
    }
    let n = pts.len();
    let mut segs = Vec::with_capacity(n);
    for i in 0..n {
        let src = pts[i];
        if is_blank(&src) {
            continue; // beam off: pen-up move, draw nothing
        }
        segs.push((src, pts[(i + 1) % n])); // wrap closes the shape
    }
    segs
}

/// Draw a pattern into `rect`, normalizing galvo space (Y increases downward, as
/// the hardware does) and honouring blanking + shape-closing via
/// `outline_segments`. If `tint` is `Some([r,g,b])` (3-bit) lit segments use that
/// colour; otherwise each segment uses its source point's own colour. Shared by
/// the editor preview, the pattern picker, and the inline timeline thumbnails.
pub fn paint_pattern(
    painter: &egui::Painter,
    rect: egui::Rect,
    points: &[PatternPoint],
    tint: Option<[u8; 3]>,
) {
    let to_screen = |p: &PatternPoint| {
        let nx = (p.x as f32 / GALVO_MAX).clamp(0.0, 1.0);
        let ny = (p.y as f32 / GALVO_MAX).clamp(0.0, 1.0);
        egui::pos2(rect.min.x + nx * rect.width(), rect.min.y + ny * rect.height())
    };
    for (src, dst) in outline_segments(points) {
        let [r, g, b] = tint.unwrap_or([src.r, src.g, src.b]);
        painter.line_segment(
            [to_screen(&src), to_screen(&dst)],
            egui::Stroke::new(1.5, egui::Color32::from_rgb(chan(r), chan(g), chan(b))),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_builds_and_ids_align() {
        let lib = library();
        assert_eq!(lib.len(), 38);
        for (i, p) in lib.iter().enumerate() {
            assert_eq!(p.id as usize, i);
            assert_eq!(p.name, PATTERN_NAMES[i]);
        }
        // Verified fact: gravestone_cross has 13 points within the galvo range.
        let gc = get(name_to_id("gravestone-cross").unwrap()).unwrap();
        assert_eq!(gc.points.len(), 13);
        for pt in &gc.points {
            assert!((0..=320).contains(&pt.x), "x out of range: {}", pt.x);
            assert!((0..=320).contains(&pt.y), "y out of range: {}", pt.y);
            assert!(pt.r <= 7 && pt.g <= 7 && pt.b <= 7);
        }
    }

    #[test]
    fn outline_blanks_and_closes() {
        let lib = library();
        let by = |n: &str| &lib[name_to_id(n).unwrap() as usize].points;

        // Horizontal lines: 5 clean left→right bars, no diagonal connectors.
        let segs = outline_segments(by("horizontal_lines_left_to_right_slow"));
        assert_eq!(segs.len(), 5);
        for (a, b) in &segs {
            assert!(!is_blank(a));
            assert_eq!(a.y, b.y, "bar should be horizontal");
            assert!(a.x < b.x, "bar runs left→right");
        }

        // Square: 4 segments forming a closed loop (last returns to the first).
        let segs = outline_segments(by("square_small"));
        assert_eq!(segs.len(), 4);
        assert_eq!((segs[3].1.x, segs[3].1.y), (segs[0].0.x, segs[0].0.y));

        // Gravestone: its two blank points break the loop (no lit segment starts
        // from a blank), leaving 13 - 2 = 11 lit segments.
        let segs = outline_segments(by("gravestone_cross"));
        assert_eq!(segs.len(), 11);
        assert!(segs.iter().all(|(a, _)| !is_blank(a)));
    }
}
