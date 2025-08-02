use anyhow::{Context, Result};
use image::GenericImageView;
use libheif_rs::{
    Channel, ColorSpace, CompressionFormat, EncoderQuality, HeifContext, Image, LibHeif, RgbChroma,
};
use log::debug;
use std::fs;
use std::path::Path;

use crate::config::HeicSettings;
use crate::file_detector::ImageFormat;

pub fn convert_to_heic_blocking(
    input_path: &Path,
    heic_settings: &HeicSettings,
) -> Result<Vec<u8>> {
    debug!("Converting image: {input_path:?}");

    // Read the input image
    let input_data = fs::read(input_path)
        .with_context(|| format!("Failed to read input image: {input_path:?}"))?;

    // Load image using the image crate
    let img = image::load_from_memory(&input_data)
        .with_context(|| format!("Failed to decode image: {input_path:?}"))?;

    // Convert to RGB8 format for HEIC encoding
    let rgb_img = img.to_rgb8();
    let (width, height) = rgb_img.dimensions();

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

pub fn estimate_heic_size(original_path: &Path, heic_settings: &HeicSettings) -> Result<u64> {
    // For estimation without actually converting, we can use heuristics
    // based on image dimensions and quality settings

    let metadata = fs::metadata(original_path)
        .with_context(|| format!("Failed to get metadata for: {original_path:?}"))?;

    let original_size = metadata.len();

    // Read just enough to get image dimensions
    let input_data = fs::read(original_path)
        .with_context(|| format!("Failed to read input image: {original_path:?}"))?;

    let img = image::load_from_memory(&input_data)
        .with_context(|| format!("Failed to decode image: {original_path:?}"))?;

    let (width, height) = img.dimensions();
    let pixel_count = width as u64 * height as u64;

    // Estimation based on quality and pixel count
    // HEIC typically achieves 50-80% compression compared to JPEG
    let quality_factor = heic_settings.quality as f64 / 100.0;
    let base_compression = 0.3; // Base compression ratio for HEIC
    let quality_adjusted_compression = base_compression + (quality_factor * 0.4);

    // Additional estimation based on original format
    let format_factor = if let Ok(Some(format)) = crate::file_detector::FileDetector::new(vec![])
        .unwrap()
        .detect_format(original_path)
    {
        match format {
            ImageFormat::Png => 0.8,  // PNG is usually larger, so more compression
            ImageFormat::Jpeg => 1.0, // JPEG baseline
            ImageFormat::Gif => 0.9,  // GIF can vary
            ImageFormat::Heic => 1.0, // Already HEIC
            _ => 1.0,
        }
    } else {
        1.0
    };

    let estimated_size =
        (original_size as f64 * quality_adjusted_compression * format_factor) as u64;

    // Ensure minimum reasonable size
    let min_size = std::cmp::max(1024, pixel_count / 100); // At least 1KB or 1 byte per 100 pixels

    Ok(std::cmp::max(estimated_size, min_size))
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
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_estimate_heic_size() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let test_file = temp_dir.path().join("test.jpg");

        // Create a realistic test image with random-ish data (less compressible)
        let mut img = image::RgbImage::new(200, 200);
        for (x, y, pixel) in img.enumerate_pixels_mut() {
            // Create a pattern that's less compressible than solid colors
            *pixel = image::Rgb([
                ((x + y) % 256) as u8,
                ((x * 2) % 256) as u8,
                ((y * 2) % 256) as u8,
            ]);
        }
        let dynamic_img = DynamicImage::ImageRgb8(img);
        dynamic_img.save_with_format(&test_file, ImageCrateFormat::Jpeg)?;

        let settings = HeicSettings {
            quality: 50,
            speed: 4,
            chroma: 420,
        };

        let estimated_size = estimate_heic_size(&test_file, &settings)?;
        assert!(estimated_size > 0);

        // For this test, just ensure the estimation is reasonable (not too large)
        let original_size = fs::metadata(&test_file)?.len();
        assert!(estimated_size < original_size * 2); // Should be within 2x of original

        Ok(())
    }

    #[test]
    fn test_is_convertible_format() {
        // These tests would need actual image files to work properly
        // For now, just test that the function doesn't panic
        let path = Path::new("test.jpg");
        let _ = is_convertible_format(path);

        let path = Path::new("test.heic");
        let _ = is_convertible_format(path);
    }
}
