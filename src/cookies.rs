//! Gobo "cookie" textures for the laser spotlights: each laser pattern
//! (bat, ghost, gravestone, …) is rasterized once into a single-channel mask so
//! a `SpotLightTexture` can project the shape as real light onto the wall.
//!
//! Only the red channel is read by the spotlight cookie sampler, and the border
//! must stay black to avoid the light leaking past the shape — both handled here.

use bevy::asset::RenderAssetUsages;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

use crate::patterns::{self, PatternPoint, GALVO_MAX};

/// Cookie resolution (square).
const COOKIE_SIZE: u32 = 256;
/// Fraction of the image kept as a black border (no light leak past the shape).
const BORDER: f32 = 0.1;

/// One generated cookie per laser pattern, indexed by pattern id (parallel to
/// `patterns::library()`).
#[derive(Resource, Default)]
pub struct PatternCookies {
    pub images: Vec<Handle<Image>>,
}

/// Rasterize a pattern's lit outline into an R8 mask (white lines on black).
fn build_cookie(points: &[PatternPoint]) -> Image {
    let n = COOKIE_SIZE as usize;
    let mut data = vec![0u8; n * n];
    let span = 1.0 - 2.0 * BORDER;
    let to_px = |x: i16, y: i16| -> (f32, f32) {
        let u = BORDER + (x as f32 / GALVO_MAX).clamp(0.0, 1.0) * span;
        let v = BORDER + (y as f32 / GALVO_MAX).clamp(0.0, 1.0) * span;
        (u * n as f32, v * n as f32)
    };
    let mut plot = |px: i32, py: i32| {
        // 3x3 brush so thin lines survive the projection.
        for dy in -1..=1 {
            for dx in -1..=1 {
                let x = px + dx;
                let y = py + dy;
                if x >= 0 && y >= 0 && (x as usize) < n && (y as usize) < n {
                    data[y as usize * n + x as usize] = 255;
                }
            }
        }
    };
    for (src, dst) in patterns::outline_segments(points) {
        let (x0, y0) = to_px(src.x, src.y);
        let (x1, y1) = to_px(dst.x, dst.y);
        let steps = (x1 - x0).abs().max((y1 - y0).abs()).ceil().max(1.0) as i32;
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            let x = (x0 + (x1 - x0) * t).round() as i32;
            let y = (y0 + (y1 - y0) * t).round() as i32;
            plot(x, y);
        }
    }
    Image::new(
        Extent3d {
            width: COOKIE_SIZE,
            height: COOKIE_SIZE,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::R8Unorm,
        RenderAssetUsages::RENDER_WORLD,
    )
}

/// Startup: build a cookie for every pattern in the library.
pub fn generate_cookies(mut images: ResMut<Assets<Image>>, mut cookies: ResMut<PatternCookies>) {
    cookies.images = patterns::library()
        .iter()
        .map(|p| images.add(build_cookie(&p.points)))
        .collect();
    info!("generated {} laser gobo cookies", cookies.images.len());
}
