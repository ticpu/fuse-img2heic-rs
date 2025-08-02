use anyhow::{Context, Result};
use log::debug;
use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::config::SourcePath;

#[derive(Debug, Clone, PartialEq)]
pub enum ImageFormat {
    Jpeg,
    Png,
    Gif,
    Heic,
    Webp,
    Bmp,
    Tiff,
}

impl ImageFormat {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_lowercase().as_str() {
            "jpg" | "jpeg" => Some(Self::Jpeg),
            "png" => Some(Self::Png),
            "gif" => Some(Self::Gif),
            "heic" | "heif" => Some(Self::Heic),
            "webp" => Some(Self::Webp),
            "bmp" => Some(Self::Bmp),
            "tif" | "tiff" => Some(Self::Tiff),
            _ => None,
        }
    }

    pub fn from_content(data: &[u8]) -> Option<Self> {
        let kind = infer::get(data)?;

        match kind.mime_type() {
            "image/jpeg" => Some(Self::Jpeg),
            "image/png" => Some(Self::Png),
            "image/gif" => Some(Self::Gif),
            "image/heic" => Some(Self::Heic),
            "image/webp" => Some(Self::Webp),
            "image/bmp" => Some(Self::Bmp),
            "image/tiff" => Some(Self::Tiff),
            _ => None,
        }
    }

    pub fn should_convert(&self) -> bool {
        match self {
            Self::Jpeg | Self::Png | Self::Gif | Self::Webp | Self::Bmp | Self::Tiff => true,
            Self::Heic => false, // Already in target format
        }
    }
}

pub struct FileDetector {
    filename_patterns: Vec<Regex>,
}

impl FileDetector {
    pub fn new(patterns: Vec<String>) -> Result<Self> {
        let mut filename_patterns = Vec::new();

        for pattern in patterns {
            let regex = Regex::new(&pattern)
                .with_context(|| format!("Invalid regex pattern: {pattern}"))?;
            filename_patterns.push(regex);
        }

        Ok(Self { filename_patterns })
    }

    pub fn is_image_file(&self, path: &Path) -> bool {
        // First check by filename pattern
        if let Some(filename) = path.file_name().and_then(|n| n.to_str()) {
            if self
                .filename_patterns
                .iter()
                .any(|regex| regex.is_match(filename))
            {
                return true;
            }
        }

        // If filename doesn't match, try content detection for existing files
        if path.exists() && path.is_file() {
            if let Ok(mut file) = fs::File::open(path) {
                let mut buffer = [0; 512]; // Read first 512 bytes for detection
                if let Ok(bytes_read) = std::io::Read::read(&mut file, &mut buffer) {
                    if bytes_read > 0 {
                        return ImageFormat::from_content(&buffer[..bytes_read]).is_some();
                    }
                }
            }
        }

        false
    }

    pub fn detect_format(&self, path: &Path) -> Result<Option<ImageFormat>> {
        // Try content detection first (more reliable)
        if path.exists() && path.is_file() {
            let mut file =
                fs::File::open(path).with_context(|| format!("Failed to open file: {path:?}"))?;

            let mut buffer = [0; 512];
            let bytes_read = std::io::Read::read(&mut file, &mut buffer)
                .with_context(|| format!("Failed to read file: {path:?}"))?;

            if bytes_read > 0 {
                if let Some(format) = ImageFormat::from_content(&buffer[..bytes_read]) {
                    debug!("Detected format by content: {path:?} -> {format:?}");
                    return Ok(Some(format));
                }
            }
        }

        // Fallback to extension detection
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if let Some(format) = ImageFormat::from_extension(ext) {
                debug!("Detected format by extension: {path:?} -> {format:?}");
                return Ok(Some(format));
            }
        }

        Ok(None)
    }

    pub fn discover_images(&self, source_paths: &[SourcePath]) -> Result<Vec<PathBuf>> {
        let mut image_files = Vec::new();

        for source_path in source_paths {
            debug!(
                "Discovering images in: {:?} (recursive: {})",
                source_path.path, source_path.recursive
            );

            if !source_path.path.exists() {
                log::warn!("Source path does not exist: {:?}", source_path.path);
                continue;
            }

            if source_path.path.is_file() {
                if self.is_image_file(&source_path.path) {
                    image_files.push(source_path.path.clone());
                }
                continue;
            }

            // Walk directory
            let walker = if source_path.recursive {
                WalkDir::new(&source_path.path)
            } else {
                WalkDir::new(&source_path.path).max_depth(1)
            };

            for entry in walker {
                match entry {
                    Ok(entry) => {
                        let path = entry.path();
                        if path.is_file() && self.is_image_file(path) {
                            image_files.push(path.to_path_buf());
                        }
                    }
                    Err(e) => {
                        log::warn!("Error walking directory: {e}");
                    }
                }
            }
        }

        debug!("Discovered {} image files", image_files.len());
        Ok(image_files)
    }

    pub fn get_virtual_path(
        &self,
        real_path: &Path,
        source_paths: &[SourcePath],
    ) -> Option<PathBuf> {
        // Find which source path this file belongs to
        for source_path in source_paths {
            if let Ok(relative) = real_path.strip_prefix(&source_path.path) {
                // Convert extension to .heic if it's a convertible format
                if let Ok(Some(format)) = self.detect_format(real_path) {
                    if format.should_convert() {
                        let mut virtual_path = PathBuf::from(relative);
                        virtual_path.set_extension("heic");
                        return Some(virtual_path);
                    }
                }
                // Return as-is for non-convertible formats
                return Some(relative.to_path_buf());
            }
        }

        None
    }

    pub fn get_real_path(
        &self,
        virtual_path: &Path,
        source_paths: &[SourcePath],
    ) -> Option<PathBuf> {
        // Try to find the real file by checking all possible extensions
        for source_path in source_paths {
            let base_path = source_path.path.join(virtual_path);

            // If requesting a .heic file, try to find the original with different extensions
            if virtual_path.extension().is_some_and(|ext| ext == "heic") {
                let stem = base_path.file_stem()?;
                let parent = base_path.parent()?;

                let extensions = ["jpg", "jpeg", "png", "gif", "webp", "bmp", "tiff"];
                for ext in &extensions {
                    let real_path = parent.join(format!("{}.{}", stem.to_str()?, ext));
                    if real_path.exists() && self.is_image_file(&real_path) {
                        return Some(real_path);
                    }
                }
            } else {
                // Direct mapping for non-heic files
                if base_path.exists() && self.is_image_file(&base_path) {
                    return Some(base_path);
                }
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_format_detection() {
        assert_eq!(ImageFormat::from_extension("jpg"), Some(ImageFormat::Jpeg));
        assert_eq!(ImageFormat::from_extension("PNG"), Some(ImageFormat::Png));
        assert_eq!(ImageFormat::from_extension("gif"), Some(ImageFormat::Gif));
        assert_eq!(ImageFormat::from_extension("txt"), None);
    }

    #[test]
    fn test_should_convert() {
        assert!(ImageFormat::Jpeg.should_convert());
        assert!(ImageFormat::Png.should_convert());
        assert!(!ImageFormat::Heic.should_convert());
    }

    #[test]
    fn test_file_detector() -> Result<()> {
        let detector = FileDetector::new(vec![r".*\.(jpg|jpeg|png|gif)$".to_string()])?;

        let temp_dir = TempDir::new()?;
        let jpg_file = temp_dir.path().join("test.jpg");
        let txt_file = temp_dir.path().join("test.txt");

        fs::write(&jpg_file, b"test")?;
        fs::write(&txt_file, b"test")?;

        assert!(detector.is_image_file(&jpg_file));
        assert!(!detector.is_image_file(&txt_file));

        Ok(())
    }
}
