/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use app_units::Au;
use euclid::Vector2D;
use net_traits::image_cache::MaskImageMode;
use style::computed_values::background_clip::single_value::T as Clip;
use style::computed_values::background_origin::single_value::T as Origin;
use style::values::computed::background::BackgroundSize as Size;
use style::values::computed::image::Gradient;
use style::values::computed::{LengthPercentage, MaskComposite, MaskMode};
use webrender_api::units::{DeviceIntSize, LayoutPoint, LayoutRect, LayoutSize};
use webrender_api::{self as wr, ClipChainId, ExtendMode, SpatialId};

use crate::context::ResolvedImage;
use crate::display_list::background::{self, get_cyclic};
use crate::display_list::gradient::{self, LinearGradientParams};
use crate::display_list::{BuilderForBoxFragment, DisplayListBuilder, StackingContext};
use crate::fragment_tree::BoxFragment;
use crate::replaced::NaturalSizes;
use crate::style_ext::ComputedValuesExt;

const MAX_AXIS_ALIGNED_MASK_DIM: f32 = 512.0;
const MAX_DIAGONAL_MASK_DIM: f32 = 256.0;

pub(super) fn resolve_mask_mode(mode: &MaskMode) -> MaskImageMode {
    match mode {
        MaskMode::Luminance => MaskImageMode::Luminance,
        MaskMode::Alpha | MaskMode::MatchSource => MaskImageMode::Alpha,
    }
}

fn hash_f32<H: Hasher>(value: f32, state: &mut H) {
    let normalized = if value == 0.0 { 0.0 } else { value };
    normalized.to_bits().hash(state);
}

pub(super) fn compute_gradient_raster_size(
    tile_size: LayoutSize,
    scale: f32,
    start_point: LayoutPoint,
    end_point: LayoutPoint,
) -> DeviceIntSize {
    let dx = end_point.x - start_point.x;
    let dy = end_point.y - start_point.y;
    if dx == 0.0 && dy == 0.0 {
        return DeviceIntSize::new(1, 1);
    }

    if dy.abs() <= dx.abs() * 1e-5 {
        let width = (tile_size.width * scale)
            .round()
            .clamp(1.0, MAX_AXIS_ALIGNED_MASK_DIM) as i32;
        DeviceIntSize::new(width, 1)
    } else if dx.abs() <= dy.abs() * 1e-5 {
        let height = (tile_size.height * scale)
            .round()
            .clamp(1.0, MAX_AXIS_ALIGNED_MASK_DIM) as i32;
        DeviceIntSize::new(1, height)
    } else {
        let width = (tile_size.width * scale)
            .round()
            .clamp(1.0, MAX_DIAGONAL_MASK_DIM) as i32;
        let height = (tile_size.height * scale)
            .round()
            .clamp(1.0, MAX_DIAGONAL_MASK_DIM) as i32;
        DeviceIntSize::new(width, height)
    }
}

pub(super) fn hash_linear_gradient_mask(
    params: &LinearGradientParams,
    mode: MaskImageMode,
    raster_size: DeviceIntSize,
    tile_size: LayoutSize,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    hash_f32(params.start_point.x, &mut hasher);
    hash_f32(params.start_point.y, &mut hasher);
    hash_f32(params.end_point.x, &mut hasher);
    hash_f32(params.end_point.y, &mut hasher);
    match params.extend_mode {
        ExtendMode::Clamp => 0u8.hash(&mut hasher),
        ExtendMode::Repeat => 1u8.hash(&mut hasher),
    }
    params.stops.len().hash(&mut hasher);
    for stop in &params.stops {
        hash_f32(stop.offset, &mut hasher);
        hash_f32(stop.color.r, &mut hasher);
        hash_f32(stop.color.g, &mut hasher);
        hash_f32(stop.color.b, &mut hasher);
        hash_f32(stop.color.a, &mut hasher);
    }
    mode.hash(&mut hasher);
    raster_size.width.hash(&mut hasher);
    raster_size.height.hash(&mut hasher);
    hash_f32(tile_size.width, &mut hasher);
    hash_f32(tile_size.height, &mut hasher);
    hasher.finish()
}

fn premultiplied_rgba(color: wr::ColorF) -> [f32; 4] {
    let a = color.a.clamp(0.0, 1.0);
    [
        color.r.clamp(0.0, 1.0) * a,
        color.g.clamp(0.0, 1.0) * a,
        color.b.clamp(0.0, 1.0) * a,
        a,
    ]
}

fn sample_linear_stops(
    stops: &[wr::GradientStop],
    extend_mode: ExtendMode,
    t: f32,
) -> [f32; 4] {
    let Some(first) = stops.first() else {
        return [0.0; 4];
    };
    let Some(last) = stops.last() else {
        return [0.0; 4];
    };

    let t = match extend_mode {
        ExtendMode::Clamp => t,
        ExtendMode::Repeat => {
            let period = last.offset - first.offset;
            if period <= 1e-6 {
                return premultiplied_rgba(last.color);
            }
            let wrapped = first.offset + (t - first.offset).rem_euclid(period);
            if wrapped >= last.offset {
                first.offset
            } else {
                wrapped
            }
        },
    };

    if t <= first.offset {
        return premultiplied_rgba(first.color);
    }
    if t >= last.offset {
        return premultiplied_rgba(last.color);
    }

    let idx = stops.partition_point(|stop| stop.offset <= t);
    let s0 = &stops[idx - 1];
    let s1 = &stops[idx];
    let span = s1.offset - s0.offset;
    if span <= 0.0 {
        return premultiplied_rgba(s0.color);
    }

    let frac = ((t - s0.offset) / span).clamp(0.0, 1.0);
    let p0 = premultiplied_rgba(s0.color);
    let p1 = premultiplied_rgba(s1.color);
    [
        p0[0] + (p1[0] - p0[0]) * frac,
        p0[1] + (p1[1] - p0[1]) * frac,
        p0[2] + (p1[2] - p0[2]) * frac,
        p0[3] + (p1[3] - p0[3]) * frac,
    ]
}

pub(super) fn rasterize_linear_gradient_r8(
    params: &LinearGradientParams,
    mode: MaskImageMode,
    raster_size: DeviceIntSize,
    tile_size: LayoutSize,
) -> Vec<u8> {
    let width = raster_size.width.max(1) as usize;
    let height = raster_size.height.max(1) as usize;
    let mut bytes = Vec::with_capacity(width * height);

    let dx = params.end_point.x - params.start_point.x;
    let dy = params.end_point.y - params.start_point.y;
    let len_sq = dx * dx + dy * dy;

    for py in 0..height {
        let y = ((py as f32 + 0.5) / height as f32) * tile_size.height;
        let y_term = (y - params.start_point.y) * dy;
        for px in 0..width {
            let t = if len_sq <= 1e-12 {
                0.0
            } else {
                let x = ((px as f32 + 0.5) / width as f32) * tile_size.width;
                ((x - params.start_point.x) * dx + y_term) / len_sq
            };

            let premul = sample_linear_stops(&params.stops, params.extend_mode, t);
            let value = match mode {
                MaskImageMode::Alpha => premul[3],
                MaskImageMode::Luminance => {
                    0.2126 * premul[0] + 0.7152 * premul[1] + 0.0722 * premul[2]
                },
            };
            bytes.push((value.clamp(0.0, 1.0) * 255.0).round() as u8);
        }
    }

    bytes
}

pub(super) fn compute_mask_rect(
    positioning_area: LayoutRect,
    size: &Size,
    pos_x: &LengthPercentage,
    pos_y: &LengthPercentage,
    natural_sizes: NaturalSizes,
) -> Option<LayoutRect> {
    let area_size = positioning_area.size();
    if area_size.width <= 0.0 ||
        area_size.height <= 0.0 ||
        !area_size.width.is_finite() ||
        !area_size.height.is_finite()
    {
        return None;
    }

    let tile_size = background::compute_tile_size(size, area_size, natural_sizes);
    if tile_size.width <= 0.0 ||
        tile_size.height <= 0.0 ||
        !tile_size.width.is_finite() ||
        !tile_size.height.is_finite()
    {
        return None;
    }

    let x = pos_x
        .to_used_value(Au::from_f32_px(area_size.width - tile_size.width))
        .to_f32_px();
    let y = pos_y
        .to_used_value(Au::from_f32_px(area_size.height - tile_size.height))
        .to_f32_px();

    Some(LayoutRect::from_origin_and_size(
        positioning_area.min + Vector2D::new(x, y),
        LayoutSize::new(tile_size.width, tile_size.height),
    ))
}

pub(super) fn build_mask_clip_chain(
    builder: &mut DisplayListBuilder,
    stacking_context: &StackingContext,
    fragment: &Arc<BoxFragment>,
    spatial_id: SpatialId,
    parent_clip_chain_id: Option<ClipChainId>,
) -> Option<ClipChainId> {
    let fragment_with_style = fragment.with_style();
    let style = fragment_with_style.style();
    if !style.has_mask_image() {
        return None;
    }

    let svg = style.get_svg();
    let fragment_builder =
        BuilderForBoxFragment::new(&fragment_with_style, stacking_context.containing_block_origin);
    let node = fragment.base.tag.map(|tag| tag.node);
    let scale = builder.device_pixel_ratio.get();

    let mut clip_ids = Vec::new();
    for (index, image) in svg.mask_image.0.iter().enumerate() {
        let Ok(resolved) = builder.image_resolver.resolve_image(node, image) else {
            continue;
        };

        let natural_sizes = match &resolved {
            ResolvedImage::Image { size, .. } => {
                NaturalSizes::from_width_and_height(size.width, size.height)
            },
            ResolvedImage::Gradient(Gradient::Linear { .. }) => NaturalSizes::empty(),
            _ => continue,
        };

        let positioning_area = match get_cyclic(&svg.mask_origin.0, index) {
            Origin::ContentBox => *fragment_builder.content_rect(),
            Origin::PaddingBox => *fragment_builder.padding_rect(),
            Origin::BorderBox => fragment_builder.border_rect,
        };
        let mask_clip_rect = match get_cyclic(&svg.mask_clip.0, index) {
            Clip::ContentBox => Some(*fragment_builder.content_rect()),
            Clip::PaddingBox => Some(*fragment_builder.padding_rect()),
            Clip::BorderBox => Some(fragment_builder.border_rect),
            Clip::BorderArea | Clip::Text => None,
        };

        let Some(mask_rect) = compute_mask_rect(
            positioning_area,
            get_cyclic(&svg.mask_size.0, index),
            get_cyclic(&svg.mask_position_x.0, index),
            get_cyclic(&svg.mask_position_y.0, index),
            natural_sizes,
        ) else {
            continue;
        };

        let tile_size = mask_rect.size();
        let mode = resolve_mask_mode(get_cyclic(&svg.mask_mode.0, index));
        let mask_key = match resolved {
            ResolvedImage::Image {
                image: cached_image,
                ..
            } => {
                let requested_device_size = DeviceIntSize::new(
                    (tile_size.width * scale).round().max(1.0) as i32,
                    (tile_size.height * scale).round().max(1.0) as i32,
                );
                let Some(mask_key) = builder.image_resolver.mask_image_key_from_cached_image(
                    &cached_image,
                    requested_device_size,
                    node,
                    mode,
                ) else {
                    continue;
                };
                mask_key
            },
            ResolvedImage::Gradient(Gradient::Linear {
                items,
                direction,
                color_interpolation_method,
                flags,
                compat_mode: _,
            }) => {
                let params = gradient::resolve_linear_gradient(
                    style,
                    items,
                    direction,
                    color_interpolation_method,
                    *flags,
                    tile_size,
                );
                if params.stops.is_empty() {
                    continue;
                }

                let raster_size = compute_gradient_raster_size(
                    tile_size,
                    scale,
                    params.start_point,
                    params.end_point,
                );
                let cache_key = hash_linear_gradient_mask(&params, mode, raster_size, tile_size);
                let Some(mask_key) = builder
                    .image_resolver
                    .get_r8_mask_image_key(cache_key)
                    .or_else(|| {
                        let bytes =
                            rasterize_linear_gradient_r8(&params, mode, raster_size, tile_size);
                        builder
                            .image_resolver
                            .get_or_create_r8_mask_image_key(cache_key, raster_size, &bytes)
                    })
                else {
                    continue;
                };
                mask_key
            },
            _ => continue,
        };

        clip_ids.push(builder.wr().define_clip_image_mask(
            spatial_id,
            wr::ImageMask {
                image: mask_key,
                rect: mask_rect,
            },
            &[],
            wr::FillRule::Nonzero,
        ));
        if let Some(clip_rect) = mask_clip_rect &&
            !clip_rect.contains_box(&mask_rect)
        {
            clip_ids.push(builder.wr().define_clip_rect(spatial_id, clip_rect));
        }

        // WebRender clip chains intersect chained clip items; avoid falsely intersecting
        // multiple layers whose CSS `mask-composite` is not `intersect`.
        if index + 1 < svg.mask_image.0.len() &&
            *get_cyclic(&svg.mask_composite.0, index) != MaskComposite::Intersect
        {
            break;
        }
    }

    if clip_ids.is_empty() {
        return None;
    }

    Some(builder.wr().define_clip_chain(parent_clip_chain_id, clip_ids))
}

#[cfg(test)]
mod tests {
    use euclid::Point2D;
    use style::values::computed::CSSPixelLength;
    use style::values::generics::NonNegative;
    use style::values::generics::length::GenericLengthPercentageOrAuto;

    use super::*;

    #[test]
    fn mask_rect_resolves_explicit_size_contain_cover_auto_and_zero_guard() {
        let box_20 = LayoutRect::from_origin_and_size(Point2D::new(10.0, 20.0), LayoutSize::new(20.0, 20.0));
        let px_16 = Size::ExplicitSize {
            width: GenericLengthPercentageOrAuto::LengthPercentage(
                NonNegative(LengthPercentage::new_length(CSSPixelLength::new(16.0))),
            ),
            height: GenericLengthPercentageOrAuto::Auto,
        };
        let center = LengthPercentage::new_percent(style::values::computed::Percentage(0.5));
        let natural_square = || NaturalSizes::from_width_and_height(20.0, 20.0);

        let rect = compute_mask_rect(box_20, &px_16, &center, &center, natural_square()).unwrap();
        assert_eq!(rect.min, Point2D::new(12.0, 22.0));
        assert_eq!(rect.size(), LayoutSize::new(16.0, 16.0));

        let box_wide = LayoutRect::from_origin_and_size(Point2D::zero(), LayoutSize::new(40.0, 20.0));
        let rect_contain = compute_mask_rect(box_wide, &Size::Contain, &center, &center, natural_square()).unwrap();
        assert_eq!(rect_contain.size(), LayoutSize::new(20.0, 20.0));
        assert_eq!(rect_contain.min, Point2D::new(10.0, 0.0));

        let rect_cover = compute_mask_rect(box_wide, &Size::Cover, &center, &center, natural_square()).unwrap();
        assert_eq!(rect_cover.size(), LayoutSize::new(40.0, 40.0));
        assert_eq!(rect_cover.min, Point2D::new(0.0, -10.0));

        let rect_auto = compute_mask_rect(box_wide, &Size::auto(), &center, &center, natural_square()).unwrap();
        assert_eq!(rect_auto.size(), LayoutSize::new(20.0, 20.0));

        let zero_size = Size::ExplicitSize {
            width: GenericLengthPercentageOrAuto::LengthPercentage(
                NonNegative(LengthPercentage::new_length(CSSPixelLength::new(0.0))),
            ),
            height: GenericLengthPercentageOrAuto::Auto,
        };
        assert!(compute_mask_rect(box_20, &zero_size, &center, &center, natural_square()).is_none());
    }

    #[test]
    fn mask_mode_match_source_defaults_to_alpha_for_images() {
        assert_eq!(resolve_mask_mode(&MaskMode::MatchSource), MaskImageMode::Alpha);
        assert_eq!(resolve_mask_mode(&MaskMode::Alpha), MaskImageMode::Alpha);
        assert_eq!(resolve_mask_mode(&MaskMode::Luminance), MaskImageMode::Luminance);
    }

    #[test]
    fn linear_gradient_mask_rasterizes_horizontal_vertical_diagonal_alpha_and_luminance() {
        // Horizontal gradient (to right): height collapses to 1, width clamps to 512.
        let tile_h = LayoutSize::new(4.0, 20.0);
        let params_h = LinearGradientParams {
            start_point: LayoutPoint::new(0.0, 10.0),
            end_point: LayoutPoint::new(4.0, 10.0),
            extend_mode: ExtendMode::Clamp,
            stops: vec![
                wr::GradientStop {
                    offset: 0.0,
                    color: wr::ColorF::new(1.0, 1.0, 1.0, 0.0),
                },
                wr::GradientStop {
                    offset: 1.0,
                    color: wr::ColorF::new(1.0, 1.0, 1.0, 1.0),
                },
            ],
        };
        let size_h = compute_gradient_raster_size(tile_h, 1.0, params_h.start_point, params_h.end_point);
        assert_eq!(size_h, DeviceIntSize::new(4, 1));
        assert_eq!(
            compute_gradient_raster_size(
                LayoutSize::new(1200.0, 800.0),
                1.0,
                LayoutPoint::new(0.0, 400.0),
                LayoutPoint::new(1200.0, 400.0),
            ),
            DeviceIntSize::new(512, 1),
        );
        let bytes_h_alpha =
            rasterize_linear_gradient_r8(&params_h, MaskImageMode::Alpha, size_h, tile_h);
        // Pixel centers at 0.125, 0.375, 0.625, 0.875 -> ~32, 96, 159, 223.
        assert_eq!(bytes_h_alpha, vec![32, 96, 159, 223]);

        // Luminance mode on opaque red -> green -> white.
        let params_lum = LinearGradientParams {
            start_point: LayoutPoint::new(0.0, 10.0),
            end_point: LayoutPoint::new(3.0, 10.0),
            extend_mode: ExtendMode::Clamp,
            stops: vec![
                wr::GradientStop {
                    offset: 1.0 / 6.0,
                    color: wr::ColorF::new(1.0, 0.0, 0.0, 1.0),
                },
                wr::GradientStop {
                    offset: 0.5,
                    color: wr::ColorF::new(0.0, 1.0, 0.0, 1.0),
                },
                wr::GradientStop {
                    offset: 5.0 / 6.0,
                    color: wr::ColorF::new(1.0, 1.0, 1.0, 0.5),
                },
            ],
        };
        let size_lum = DeviceIntSize::new(3, 1);
        let bytes_h_lum = rasterize_linear_gradient_r8(
            &params_lum,
            MaskImageMode::Luminance,
            size_lum,
            LayoutSize::new(3.0, 20.0),
        );
        assert_eq!(bytes_h_lum, vec![54, 182, 128]);

        // Vertical gradient (to bottom): width collapses to 1.
        let tile_v = LayoutSize::new(40.0, 4.0);
        let params_v = LinearGradientParams {
            start_point: LayoutPoint::new(20.0, 0.0),
            end_point: LayoutPoint::new(20.0, 4.0),
            extend_mode: ExtendMode::Clamp,
            stops: vec![
                wr::GradientStop {
                    offset: 0.0,
                    color: wr::ColorF::new(0.0, 0.0, 0.0, 1.0),
                },
                wr::GradientStop {
                    offset: 1.0,
                    color: wr::ColorF::new(1.0, 1.0, 1.0, 0.0),
                },
            ],
        };
        let size_v = compute_gradient_raster_size(tile_v, 1.0, params_v.start_point, params_v.end_point);
        assert_eq!(size_v, DeviceIntSize::new(1, 4));
        let bytes_v_alpha =
            rasterize_linear_gradient_r8(&params_v, MaskImageMode::Alpha, size_v, tile_v);
        assert_eq!(bytes_v_alpha, vec![223, 159, 96, 32]);
        let bytes_v_lum =
            rasterize_linear_gradient_r8(&params_v, MaskImageMode::Luminance, size_v, tile_v);
        // Stop 0 is opaque black (lum 0), stop 1 is transparent white (premul lum 0).
        assert_eq!(bytes_v_lum, vec![0, 0, 0, 0]);

        // Diagonal gradient: both axes preserved and capped at 256.
        let tile_d = LayoutSize::new(3.0, 3.0);
        let params_d = LinearGradientParams {
            start_point: LayoutPoint::new(0.0, 0.0),
            end_point: LayoutPoint::new(3.0, 3.0),
            extend_mode: ExtendMode::Clamp,
            stops: vec![
                wr::GradientStop {
                    offset: 0.0,
                    color: wr::ColorF::new(1.0, 1.0, 1.0, 0.0),
                },
                wr::GradientStop {
                    offset: 1.0,
                    color: wr::ColorF::new(1.0, 1.0, 1.0, 1.0),
                },
            ],
        };
        let size_d = compute_gradient_raster_size(tile_d, 1.0, params_d.start_point, params_d.end_point);
        assert_eq!(size_d, DeviceIntSize::new(3, 3));
        assert_eq!(
            compute_gradient_raster_size(
                LayoutSize::new(400.0, 400.0),
                2.0,
                LayoutPoint::new(0.0, 0.0),
                LayoutPoint::new(400.0, 400.0),
            ),
            DeviceIntSize::new(256, 256),
        );
        let bytes_d_alpha =
            rasterize_linear_gradient_r8(&params_d, MaskImageMode::Alpha, size_d, tile_d);
        let bytes_d_lum =
            rasterize_linear_gradient_r8(&params_d, MaskImageMode::Luminance, size_d, tile_d);
        // White stops with alpha 0->1 match in Alpha and Luminance modes.
        assert_eq!(bytes_d_alpha, bytes_d_lum);
        // Top-left (0,0) < center (1,1) == 128 < bottom-right (2,2).
        assert!(bytes_d_alpha[0] < bytes_d_alpha[4]);
        assert_eq!(bytes_d_alpha[4], 128);
        assert!(bytes_d_alpha[4] < bytes_d_alpha[8]);

        // Repeating gradient wraps across periods.
        let params_repeat = LinearGradientParams {
            start_point: LayoutPoint::new(0.0, 5.0),
            end_point: LayoutPoint::new(4.0, 5.0),
            extend_mode: ExtendMode::Repeat,
            stops: vec![
                wr::GradientStop {
                    offset: 0.0,
                    color: wr::ColorF::new(0.0, 0.0, 0.0, 0.0),
                },
                wr::GradientStop {
                    offset: 0.5,
                    color: wr::ColorF::new(0.0, 0.0, 0.0, 1.0),
                },
            ],
        };
        let bytes_repeat = rasterize_linear_gradient_r8(
            &params_repeat,
            MaskImageMode::Alpha,
            DeviceIntSize::new(4, 1),
            LayoutSize::new(4.0, 10.0),
        );
        // t = 0.125, 0.375, 0.625 (->0.125), 0.875 (->0.375) => [64, 191, 64, 191].
        assert_eq!(bytes_repeat, vec![64, 191, 64, 191]);

        // Deterministic cache hashing distinguishes modes and gradient params.
        let key_alpha = hash_linear_gradient_mask(&params_d, MaskImageMode::Alpha, size_d, tile_d);
        let key_alpha_again =
            hash_linear_gradient_mask(&params_d, MaskImageMode::Alpha, size_d, tile_d);
        let key_lum =
            hash_linear_gradient_mask(&params_d, MaskImageMode::Luminance, size_d, tile_d);
        assert_eq!(key_alpha, key_alpha_again);
        assert_ne!(key_alpha, key_lum);
    }
}
