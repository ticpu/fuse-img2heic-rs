use anyhow::{Context, Result};
use image::DynamicImage;
use libheif_rs::{
    Channel, ColorSpace, CompressionFormat, EncoderQuality, HeifContext, Image, LibHeif, RgbChroma,
};
use log::debug;
use std::fs;
use std::path::Path;

use crate::config::HeicSettings;

fn decode_heic_with_libheif(input_data: &[u8]) -> Result<DynamicImage> {
    let lib_heif = LibHeif::new();

    // Read HEIC data from bytes
    let ctx = HeifContext::read_from_bytes(input_data).context("Failed to read HEIC data")?;

    // Get primary image handle
    let handle = ctx
        .primary_image_handle()
        .context("Failed to get primary image handle")?;

    // Decode the image to RGB format
    let image = lib_heif
        .decode(&handle, ColorSpace::Rgb(RgbChroma::Rgb), None)
        .context("Failed to decode HEIC image")?;

    // Get image dimensions
    let width = image.width();
    let height = image.height();

    debug!("Decoded HEIC image: {width}x{height}");

    // Get pixel data from interleaved RGB planes
    let planes = image.planes();
    let interleaved_plane = planes
        .interleaved
        .ok_or_else(|| anyhow::anyhow!("No interleaved plane available"))?;

    // Create RGB image buffer from the plane data
    let mut rgb_data = Vec::with_capacity((width * height * 3) as usize);

    // Copy RGB data accounting for stride
    for y in 0..height {
        let row_start = (y * interleaved_plane.stride as u32) as usize;
        let row_end = row_start + (width * 3) as usize;

        if row_end <= interleaved_plane.data.len() {
            rgb_data.extend_from_slice(&interleaved_plane.data[row_start..row_end]);
        } else {
            anyhow::bail!("Invalid image data: row {} extends beyond data buffer", y);
        }
    }

    // Create DynamicImage from RGB data
    let rgb_image = image::RgbImage::from_raw(width, height, rgb_data)
        .ok_or_else(|| anyhow::anyhow!("Failed to create RGB image from decoded data"))?;

    Ok(DynamicImage::ImageRgb8(rgb_image))
}

pub fn convert_to_heic_blocking(
    input_path: &Path,
    heic_settings: &HeicSettings,
) -> Result<Vec<u8>> {
    debug!("Converting image: {input_path:?}");

    // Read the input image
    let input_data = fs::read(input_path)
        .with_context(|| format!("Failed to read input image: {input_path:?}"))?;

    // Load image - use libheif for HEIC/HEIF files, image crate for others
    let img = if input_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .as_deref()
        .is_some_and(|ext| ext == "heic" || ext == "heif")
    {
        // Use libheif-rs to decode HEIC files
        decode_heic_with_libheif(&input_data)
            .with_context(|| format!("Failed to decode HEIC image: {input_path:?}"))?
    } else {
        // Use image crate for other formats
        image::load_from_memory(&input_data)
            .with_context(|| format!("Failed to decode image: {input_path:?}"))?
    };

    // Convert to RGB8 format for HEIC encoding
    let mut rgb_img = img.to_rgb8();
    let (mut width, mut height) = rgb_img.dimensions();

    // Resize if image exceeds configured maximum resolution
    if heic_settings.should_resize(width, height) {
        if let Some((max_width, max_height)) = heic_settings.get_max_resolution() {
            // Calculate resize dimensions while preserving aspect ratio
            let width_ratio = max_width as f64 / width as f64;
            let height_ratio = max_height as f64 / height as f64;
            let scale_ratio = width_ratio.min(height_ratio);

            let new_width = (width as f64 * scale_ratio) as u32;
            let new_height = (height as f64 * scale_ratio) as u32;

            debug!("Resizing image from {width}x{height} to {new_width}x{new_height}");

            // Resize using the image crate's resize method
            let resized_img = image::DynamicImage::ImageRgb8(rgb_img).resize(
                new_width,
                new_height,
                image::imageops::FilterType::Lanczos3,
            );

            rgb_img = resized_img.to_rgb8();
            width = new_width;
            height = new_height;
        }
    }

    debug!("Image dimensions: {width}x{height}");

    // Create HEIF image
    let mut heif_image = Image::new(width, height, ColorSpace::Rgb(RgbChroma::C444))
        .context("Failed to create HEIF image")?;

    // Create RGB planes
    heif_image
        .create_plane(Channel::R, width, height, 8)
        .context("Failed to create R plane")?;
    heif_image
        .create_plane(Channel::G, width, height, 8)
        .context("Failed to create G plane")?;
    heif_image
        .create_plane(Channel::B, width, height, 8)
        .context("Failed to create B plane")?;

    // Fill the planes with RGB data
    {
        let mut planes = heif_image.planes_mut();
        let plane_r = planes.r.as_mut().context("R plane missing")?;
        let plane_g = planes.g.as_mut().context("G plane missing")?;
        let plane_b = planes.b.as_mut().context("B plane missing")?;

        let stride = plane_r.stride;

        // Copy RGB data to planes
        for y in 0..height {
            let row_start = (stride * y as usize).min(plane_r.data.len());
            let row_end = (row_start + width as usize).min(plane_r.data.len());

            for (x, pixel_idx) in (row_start..row_end).enumerate() {
                if x < width as usize && y < height {
                    let pixel = rgb_img.get_pixel(x as u32, y);
                    plane_r.data[pixel_idx] = pixel[0];
                    plane_g.data[pixel_idx] = pixel[1];
                    plane_b.data[pixel_idx] = pixel[2];
                }
            }
        }
    }

    // Encode the image to HEIC
    let lib_heif = LibHeif::new();
    let mut context = HeifContext::new().context("Failed to create HEIF context")?;

    let mut encoder = lib_heif
        .encoder_for_format(CompressionFormat::Hevc)
        .context("Failed to create HEVC encoder")?;

    // Map quality setting (1-100) to encoder quality
    let encoder_quality = if heic_settings.quality >= 95 {
        EncoderQuality::LossLess
    } else {
        EncoderQuality::Lossy(heic_settings.quality)
    };

    encoder
        .set_quality(encoder_quality)
        .context("Failed to set encoder quality")?;

    context
        .encode_image(&heif_image, &mut encoder, None)
        .context("Failed to encode image to HEIF")?;

    // Write to memory buffer
    let output_data = context
        .write_to_bytes()
        .context("Failed to write HEIF data to memory")?;

    debug!(
        "Converted {} bytes -> {} bytes (compression: {:.1}%)",
        input_data.len(),
        output_data.len(),
        (1.0 - output_data.len() as f64 / input_data.len() as f64) * 100.0
    );

    Ok(output_data)
}

pub fn is_convertible_format(path: &Path) -> bool {
    if let Ok(detector) = crate::file_detector::FileDetector::new(vec![]) {
        if let Ok(Some(format)) = detector.detect_format(path) {
            return format.should_convert();
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat as ImageCrateFormat};
    use tempfile::TempDir;

    #[test]
    fn test_is_convertible_format() {
        let path = Path::new("test.jpg");
        let _ = is_convertible_format(path);

        let path = Path::new("test.heic");
        let _ = is_convertible_format(path);
    }

    #[test]
    fn test_conversion_is_deterministic_jpg() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let test_file = temp_dir.path().join("test.jpg");

        // Create a test image with varied content
        let mut img = image::RgbImage::new(200, 200);
        for (x, y, pixel) in img.enumerate_pixels_mut() {
            *pixel = image::Rgb([
                ((x + y) % 256) as u8,
                ((x * 2) % 256) as u8,
                ((y * 2) % 256) as u8,
            ]);
        }
        DynamicImage::ImageRgb8(img).save_with_format(&test_file, ImageCrateFormat::Jpeg)?;

        let settings = HeicSettings {
            quality: 50,
            speed: 4,
            chroma: 420,
            max_resolution: None,
        };

        // Convert twice
        let result1 = convert_to_heic_blocking(&test_file, &settings)?;
        let result2 = convert_to_heic_blocking(&test_file, &settings)?;

        assert_eq!(
            result1, result2,
            "HEIC conversion must be deterministic - same input should produce identical output"
        );

        Ok(())
    }

    #[test]
    fn test_conversion_is_deterministic_png() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let test_file = temp_dir.path().join("test.png");

        // Create a test image with varied content
        let mut img = image::RgbImage::new(200, 200);
        for (x, y, pixel) in img.enumerate_pixels_mut() {
            *pixel = image::Rgb([
                ((x + y) % 256) as u8,
                ((x * 2) % 256) as u8,
                ((y * 2) % 256) as u8,
            ]);
        }
        DynamicImage::ImageRgb8(img).save_with_format(&test_file, ImageCrateFormat::Png)?;

        let settings = HeicSettings {
            quality: 50,
            speed: 4,
            chroma: 420,
            max_resolution: None,
        };

        // Convert twice
        let result1 = convert_to_heic_blocking(&test_file, &settings)?;
        let result2 = convert_to_heic_blocking(&test_file, &settings)?;

        assert_eq!(
            result1, result2,
            "HEIC conversion must be deterministic - same input should produce identical output"
        );

        Ok(())
    }
}
