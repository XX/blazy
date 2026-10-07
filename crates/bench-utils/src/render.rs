//! Measuring what a renderer put on screen, rather than how long it took.
//!
//! `rnd/architecture.md` §9 makes a claim that decides part of why `imaging` was
//! chosen at all:
//!
//! > Because `imaging` rasterises from curves with the final transform in hand,
//! > changing any of the three multipliers is correct without re-encoding the scene
//! > and without loss of sharpness. Neither atlas-based renderers (gpui) nor CPU
//! > tessellators (egui/lyon) have this.
//!
//! The failure mode that claim rules out is concrete: geometry rasterised once at one
//! scale and then magnified, which is what a texture atlas does. That is measurable
//! without looking at a single pixel by eye.
//!
//! # The metric
//!
//! [`sharpness`] is the mean **squared** luma gradient. The squaring is the whole
//! point. Blurring spreads one edge over several pixels, and the sum of *absolute*
//! differences across a ramp equals the single step it replaced — an absolute-gradient
//! metric is blind to blur by construction, which is a mistake worth not repeating.
//! Squaring is not: one step of 40 contributes 1600, four steps of 10 contribute 400.
//!
//! # What it cannot tell you
//!
//! The metric measures edge *width*, so a nearest-neighbour magnification scores as
//! well as a genuine re-render — both have edges one pixel wide. That is not a
//! plausible failure mode for a renderer (nobody magnifies a UI by pixel doubling),
//! but it is a way to pass the check while doing nothing, so pair it with
//! [`differing_fraction`] against a nearest-neighbour upscale: a real re-render
//! resamples, a block magnification does not.

use image::RgbaImage;
use image::imageops::{FilterType, resize};

/// Mean squared luma gradient over an image.
///
/// Higher is crisper. Only ever compare two images of the same size: the value is an
/// average over pixels, but the *content* has to match for the comparison to mean
/// anything.
pub fn sharpness(image: &RgbaImage) -> f64 {
    let (w, h) = image.dimensions();
    if w < 2 || h < 2 {
        return 0.0;
    }
    let luma = |x: u32, y: u32| {
        let [r, g, b, _] = image.get_pixel(x, y).0;
        0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b)
    };

    let mut total = 0.0;
    for y in 0..h - 1 {
        for x in 0..w - 1 {
            let here = luma(x, y);
            let dx = luma(x + 1, y) - here;
            let dy = luma(x, y + 1) - here;
            total += dx * dx + dy * dy;
        }
    }
    total / f64::from((w - 1) * (h - 1))
}

/// How much sharper `magnified` is than `original` blown up to the same size.
///
/// `original` is upscaled by a smooth filter, which is how a renderer that fixed its
/// geometry at the smaller scale would have to magnify it. A rasteriser that re-runs
/// the curves at the larger scale should come out several times sharper; one that
/// resamples a bitmap should come out at about 1.0.
///
/// Returns `None` when the ratio cannot be formed, which in practice means the
/// upscaled image had no edges at all.
pub fn sharpness_gain(original: &RgbaImage, magnified: &RgbaImage) -> Option<f64> {
    let upscaled = resize(original, magnified.width(), magnified.height(), FilterType::Triangle);
    let baseline = sharpness(&upscaled);
    if baseline <= f64::EPSILON {
        return None;
    }
    Some(sharpness(magnified) / baseline)
}

/// Fraction of pixels that differ between two images of the same size.
///
/// Used against a nearest-neighbour upscale: a genuine re-render resamples every
/// edge and differs from block magnification, so a fraction of zero would mean the
/// renderer did nothing but repeat pixels.
///
/// # Panics
///
/// If the two images are not the same size — comparing different sizes pixel by pixel
/// is always a mistake rather than a case worth handling.
pub fn differing_fraction(a: &RgbaImage, b: &RgbaImage) -> f64 {
    assert_eq!(
        a.dimensions(),
        b.dimensions(),
        "images must be the same size to compare"
    );
    let differing = a.pixels().zip(b.pixels()).filter(|(p, q)| p != q).count();
    differing as f64 / f64::from(a.width() * a.height())
}

/// `original` magnified to `size` by repeating pixels, with no resampling at all.
///
/// The "did the renderer actually do something" baseline for [`differing_fraction`].
pub fn block_magnified(original: &RgbaImage, width: u32, height: u32) -> RgbaImage {
    resize(original, width, height, FilterType::Nearest)
}

// --- MARK: comparing two frames by block (§45)

/// Side of the block two frames are compared in, in pixels.
pub const BLOCK: usize = 16;

/// How far a block's average channel may move before it is content and not antialiasing.
///
/// Derived rather than tuned. A block holds 256 pixels, so a pair of edge pixels flipping
/// between two extremes moves its mean by 2 * 255 / 256 ≈ 2; a block that lost the
/// content drawn in it moves by tens. Eight sits between the two, four times away from
/// each (§45).
pub const BLOCK_DELTA: u32 = 8;

/// Blocks of two frames whose average colour differs by more than antialiasing.
///
/// Why not simply compare the frames: the GPU does not rasterise bit-identically across
/// submissions, so exact equality is flaky by construction — measured at one pixel in two
/// runs out of three, with every layer copied and nothing drawn at all.
pub fn blocks_changed(first: &[u8], second: &[u8], width: usize) -> u64 {
    if first.len() != second.len() || width == 0 {
        return 0;
    }
    let height = first.len() / (width * 4);
    let mut changed = 0;
    for by in (0..height).step_by(BLOCK) {
        for bx in (0..width).step_by(BLOCK) {
            let (mut sums, mut count) = ([0i64; 4], 0i64);
            for y in by..(by + BLOCK).min(height) {
                for x in bx..(bx + BLOCK).min(width) {
                    let at = (y * width + x) * 4;
                    for channel in 0..4 {
                        sums[channel] += i64::from(first[at + channel]) - i64::from(second[at + channel]);
                    }
                    count += 1;
                }
            }
            if count > 0
                && sums
                    .iter()
                    .any(|sum| sum.unsigned_abs() >= u64::from(BLOCK_DELTA) * count as u64)
            {
                changed += 1;
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hard edge down the middle, at `scale` pixels per unit.
    fn edge(scale: u32) -> RgbaImage {
        let (w, h) = (16 * scale, 8 * scale);
        RgbaImage::from_fn(w, h, |x, _| {
            if x < w / 2 {
                [0, 0, 0, 255].into()
            } else {
                [255, 255, 255, 255].into()
            }
        })
    }

    /// The property the whole metric rests on, and the one an absolute-gradient
    /// metric does not have: blurring an edge has to lower the score.
    #[test]
    fn blurring_lowers_sharpness() {
        let sharp = edge(4);
        let blurred = resize(&edge(1), sharp.width(), sharp.height(), FilterType::Triangle);
        assert!(
            sharpness(&sharp) > sharpness(&blurred) * 2.0,
            "sharp {} vs blurred {}",
            sharpness(&sharp),
            sharpness(&blurred)
        );
    }

    /// Re-drawing at the larger size is what a gain above one means.
    #[test]
    fn redrawing_beats_upscaling() {
        let gain = sharpness_gain(&edge(1), &edge(4)).expect("an edge has edges");
        assert!(gain > 2.0, "gain was {gain}");
    }

    /// And the metric has to be honest when nothing was gained.
    #[test]
    fn upscaling_alone_gains_nothing() {
        let small = edge(1);
        let upscaled = resize(&small, small.width() * 4, small.height() * 4, FilterType::Triangle);
        let gain = sharpness_gain(&small, &upscaled).expect("an edge has edges");
        assert!((gain - 1.0).abs() < 0.01, "gain was {gain}");
    }

    /// The stated blind spot, stated as a test so nobody rediscovers it as a surprise.
    #[test]
    fn block_magnification_scores_as_well_as_a_redraw() {
        let small = edge(1);
        let blocky = block_magnified(&small, small.width() * 4, small.height() * 4);
        let gain = sharpness_gain(&small, &blocky).expect("an edge has edges");
        assert!(gain > 2.0, "gain was {gain}");
        assert_eq!(
            differing_fraction(&blocky, &block_magnified(&small, blocky.width(), blocky.height())),
            0.0,
            "which is what differing_fraction is for"
        );
    }

    #[test]
    fn a_flat_image_has_no_sharpness() {
        let flat = RgbaImage::from_pixel(8, 8, [7, 7, 7, 255].into());
        assert_eq!(sharpness(&flat), 0.0);
        assert_eq!(
            sharpness_gain(&flat, &RgbaImage::from_pixel(32, 32, [7, 7, 7, 255].into())),
            None
        );
    }
}
