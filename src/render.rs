//! Turns whatever a backend produced into an agent-friendly image: optionally
//! cropped, downscaled to a token-sane size, grayscale, re-encoded.

use std::{io::Cursor, str::FromStr};

use anyhow::{Context, Result, bail};
use image::{DynamicImage, ImageFormat, codecs::jpeg::JpegEncoder, imageops::FilterType};

use crate::config::OutputFormat;

#[derive(Debug, Clone)]
pub struct RenderOpts {
    pub region: Option<Region>,
    pub max_edge: u32,
    pub grayscale: bool,
    pub format: OutputFormat,
    pub quality: u8,
}

pub struct Rendered {
    pub data: Vec<u8>,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    pub source_width: u32,
    pub source_height: u32,
}

/// A part of the page as fractions of its width and height.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Region {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

pub const REGION_HELP: &str = "top, bottom, left, right, top-left, top-right, bottom-left, \
bottom-right, center, or \"x,y,w,h\" as fractions of the page (e.g. \"0,0.75,1,0.25\" is the bottom quarter)";

impl FromStr for Region {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let r = |x, y, w, h| Region { x, y, w, h };
        let region = match s.trim().to_ascii_lowercase().as_str() {
            "top" => r(0.0, 0.0, 1.0, 0.5),
            "bottom" => r(0.0, 0.5, 1.0, 0.5),
            "left" => r(0.0, 0.0, 0.5, 1.0),
            "right" => r(0.5, 0.0, 0.5, 1.0),
            "top-left" => r(0.0, 0.0, 0.5, 0.5),
            "top-right" => r(0.5, 0.0, 0.5, 0.5),
            "bottom-left" => r(0.0, 0.5, 0.5, 0.5),
            "bottom-right" => r(0.5, 0.5, 0.5, 0.5),
            "center" => r(0.25, 0.25, 0.5, 0.5),
            other => {
                let parts: Vec<f32> = other
                    .split(',')
                    .map(|p| p.trim().parse::<f32>())
                    .collect::<Result<_, _>>()
                    .map_err(|_| anyhow::anyhow!("bad region {s:?}; use {REGION_HELP}"))?;
                let [x, y, w, h] = parts[..] else {
                    bail!("bad region {s:?}; use {REGION_HELP}");
                };
                r(x, y, w, h)
            }
        };
        let Region { x, y, w, h } = region;
        let ok = |v: f32| (0.0..=1.0).contains(&v);
        if !(ok(x) && ok(y) && w > 0.0 && h > 0.0 && x + w <= 1.0001 && y + h <= 1.0001) {
            bail!(
                "region {s:?} falls outside the page; x,y,w,h must be fractions with x+w and y+h <= 1"
            );
        }
        Ok(region)
    }
}

pub fn render(bytes: &[u8], opts: &RenderOpts) -> Result<Rendered> {
    let img = image::load_from_memory(bytes)
        .context("backend output isn't a decodable image (PNG/JPEG/TIFF/WebP/GIF/BMP)")?;
    let (source_width, source_height) = (img.width(), img.height());

    let img = match opts.region {
        Some(Region { x, y, w, h }) => {
            let (sw, sh) = (source_width as f32, source_height as f32);
            let px = (x * sw).round() as u32;
            let py = (y * sh).round() as u32;
            let pw = ((w * sw).round() as u32).clamp(1, source_width - px.min(source_width - 1));
            let ph = ((h * sh).round() as u32).clamp(1, source_height - py.min(source_height - 1));
            img.crop_imm(px, py, pw, ph)
        }
        None => img,
    };

    // Downscale only: upscaling adds tokens without adding detail.
    let img = if img.width().max(img.height()) > opts.max_edge {
        img.resize(opts.max_edge, opts.max_edge, FilterType::Lanczos3)
    } else {
        img
    };

    let img = if opts.grayscale {
        DynamicImage::ImageLuma8(img.to_luma8())
    } else {
        DynamicImage::ImageRgb8(img.to_rgb8())
    };

    let mut data = Vec::new();
    let mime = match opts.format {
        OutputFormat::Jpeg => {
            JpegEncoder::new_with_quality(&mut data, opts.quality.clamp(1, 100))
                .encode_image(&img)?;
            "image/jpeg"
        }
        OutputFormat::Png => {
            img.write_to(&mut Cursor::new(&mut data), ImageFormat::Png)?;
            "image/png"
        }
    };

    Ok(Rendered {
        data,
        mime,
        width: img.width(),
        height: img.height(),
        source_width,
        source_height,
    })
}

#[cfg(test)]
pub(crate) fn test_png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
    });
    let mut out = Vec::new();
    DynamicImage::ImageRgb8(img)
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .unwrap();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(region: Option<&str>) -> RenderOpts {
        RenderOpts {
            region: region.map(|r| r.parse().unwrap()),
            max_edge: 1000,
            grayscale: true,
            format: OutputFormat::Jpeg,
            quality: 80,
        }
    }

    #[test]
    fn parses_regions() {
        assert_eq!("bottom".parse::<Region>().unwrap().y, 0.5);
        let r: Region = "0, 0.75, 1, 0.25".parse().unwrap();
        assert_eq!((r.x, r.y, r.w, r.h), (0.0, 0.75, 1.0, 0.25));
        assert!("0.5,0.5,0.6,0.1".parse::<Region>().is_err());
        assert!("middle".parse::<Region>().is_err());
        assert!("1,2,3".parse::<Region>().is_err());
    }

    #[test]
    fn downscales_full_page() {
        let out = render(&test_png(2000, 3000), &opts(None)).unwrap();
        assert_eq!((out.width, out.height), (667, 1000));
        assert_eq!((out.source_width, out.source_height), (2000, 3000));
        assert_eq!(out.mime, "image/jpeg");
        assert!(image::load_from_memory(&out.data).is_ok());
    }

    #[test]
    fn region_zooms_in_without_upscaling() {
        let out = render(&test_png(2000, 3000), &opts(Some("bottom-right"))).unwrap();
        assert_eq!((out.width, out.height), (667, 1000));
        let small = render(&test_png(400, 600), &opts(Some("top"))).unwrap();
        assert_eq!((small.width, small.height), (400, 300));
    }

    #[test]
    fn rejects_garbage() {
        assert!(render(b"not an image", &opts(None)).is_err());
    }
}
