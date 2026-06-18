//! The named gobo-projector pattern library, decoded once from
//! `dmx-projector.json`. Each entry maps a human name (e.g. `Large_Square`,
//! `Dolphin`, `Zeus_Scene`) to its 2-byte DMX `code` = `[gallery, pattern]`.
//!
//! This is a pure naming layer over `ProjectorKeyframe`'s raw `gallery`/`pattern`
//! bytes: the editor picks a name to set the bytes, and the timeline / viewport
//! labels resolve bytes back to a name. The wire/DB format stays raw bytes.
//!
//! The first byte is the gallery (`0x00` = static gobos, `0xff` = animated
//! scenes); the second is the pattern index within that gallery.

use std::collections::HashMap;
use std::sync::OnceLock;

/// One named projector pattern and the DMX bytes it selects.
pub struct ProjectorPattern {
    pub name: String,
    /// DMX byte 0 — `0x00` for gobos, `0xff` for scenes.
    pub gallery: u8,
    /// DMX byte 1 — pattern/scene index within the gallery.
    pub pattern: u8,
}

const PROJECTOR_JSON: &str = include_str!("../assets/dmx-projector.json");

/// Parse a `"0x0c"`-style hex byte.
fn parse_byte(h: &str) -> Option<u8> {
    u8::from_str_radix(h.trim().trim_start_matches("0x"), 16).ok()
}

fn build() -> Vec<ProjectorPattern> {
    // Each value is `{"code": ["0x00", "0x0c"]}`; deserialize into plain
    // collections (the crate pulls in serde_json but not serde-derive).
    let map: HashMap<String, HashMap<String, Vec<String>>> =
        serde_json::from_str(PROJECTOR_JSON).expect("assets/dmx-projector.json should parse");
    let mut out: Vec<ProjectorPattern> = map
        .into_iter()
        .filter(|(name, _)| name != "ignore") // sentinel (0xff,0xff)
        .filter_map(|(name, e)| {
            let code = e.get("code")?;
            let gallery = parse_byte(code.first()?)?;
            let pattern = parse_byte(code.get(1)?)?;
            Some(ProjectorPattern {
                name,
                gallery,
                pattern,
            })
        })
        .collect();
    // Stable, grouped order: all gobos (0x00) first, then scenes (0xff), by
    // pattern within each gallery. (HashMap iteration order is otherwise random.)
    out.sort_by(|a, b| (a.gallery, a.pattern).cmp(&(b.gallery, b.pattern)));
    out
}

/// The full pattern library, built once on first use.
pub fn library() -> &'static [ProjectorPattern] {
    static LIB: OnceLock<Vec<ProjectorPattern>> = OnceLock::new();
    LIB.get_or_init(build)
}

/// Resolve a `(gallery, pattern)` byte pair to its name, for display. Returns
/// the first match in sorted order; `None` if no named pattern uses those bytes.
pub fn name_for(gallery: u8, pattern: u8) -> Option<&'static str> {
    library()
        .iter()
        .find(|p| p.gallery == gallery && p.pattern == pattern)
        .map(|p| p.name.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_parses_and_known_codes_map() {
        let lib = library();
        // 274 named patterns (275 entries minus the "ignore" sentinel).
        assert_eq!(lib.len(), 274);
        assert_eq!(name_for(0x00, 0x0c), Some("Large_Square"));
        assert_eq!(name_for(0xff, 0x00), Some("Zeus_Scene"));
        assert_eq!(name_for(0xff, 0x0c), Some("Dolphin"));
        // "ignore" must not appear in the picker.
        assert!(lib.iter().all(|p| p.name != "ignore"));
        // Sorted: gobos (0x00) before scenes (0xff).
        assert!(lib.is_sorted_by_key(|p| (p.gallery, p.pattern)));
    }
}
