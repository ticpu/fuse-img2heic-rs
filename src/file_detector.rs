use anyhow::{Context, Result};
use log::debug;
use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};

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
            Self::Jpeg
            | Self::Png
            | Self::Gif
            | Self::Webp
            | Self::Bmp
            | Self::Tiff
            | Self::Heic => true,
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

    /// Check if a virtual path corresponds to a real directory
    pub fn is_virtual_directory(&self, virtual_path: &Path, source_paths: &[SourcePath]) -> bool {
        if virtual_path == Path::new("/") || virtual_path.as_os_str().is_empty() {
            return true;
        }

        let Ok((mount_name, subpath)) = self.parse_virtual_path(virtual_path) else {
            return false;
        };

        // Check if it's just a mount name (top-level directory)
        if subpath.as_os_str().is_empty() {
            return source_paths.iter().any(|sp| sp.mount_name == mount_name);
        }

        // Check if the real path exists and is a directory
        let Some(source_path) = source_paths.iter().find(|sp| sp.mount_name == mount_name) else {
            return false;
        };

        let real_path = source_path.path.join(subpath);
        real_path.is_dir()
    }

    /// List entries in a specific virtual directory with path exclusions (e.g., mount points)
    pub fn list_virtual_directory_with_exclusions(
        &self,
        virtual_dir: &Path,
        source_paths: &[SourcePath],
        exclude_paths: &[&Path],
    ) -> Result<Vec<(String, bool)>> {
        // (name, is_directory)
        if virtual_dir == Path::new("/") {
            return self.list_root_directory(source_paths);
        }

        let (mount_name, subpath) = self.parse_virtual_path(virtual_dir)?;
        let source_path = self.find_source_by_mount_name(&mount_name, source_paths)?;
        let real_dir = source_path.path.join(subpath);

        self.list_real_directory_with_exclusions(&real_dir, exclude_paths)
    }

    fn list_root_directory(&self, source_paths: &[SourcePath]) -> Result<Vec<(String, bool)>> {
        let mut entries = Vec::new();
        for source_path in source_paths {
            if source_path.path.exists() {
                entries.push((source_path.mount_name.clone(), true));
            }
        }
        Ok(entries)
    }

    fn parse_virtual_path<'a>(&self, virtual_dir: &'a Path) -> Result<(String, &'a Path)> {
        let mut components = virtual_dir.components();
        let mount_name = components
            .next()
            .and_then(|c| c.as_os_str().to_str())
            .ok_or_else(|| anyhow::anyhow!("Invalid virtual path"))?;
        let subpath = components.as_path();
        Ok((mount_name.to_string(), subpath))
    }

    fn find_source_by_mount_name<'a>(
        &self,
        mount_name: &str,
        source_paths: &'a [SourcePath],
    ) -> Result<&'a SourcePath> {
        source_paths
            .iter()
            .find(|sp| sp.mount_name == mount_name)
            .ok_or_else(|| anyhow::anyhow!("Mount name not found: {}", mount_name))
    }

    fn list_real_directory_with_exclusions(
        &self,
        real_dir: &Path,
        exclude_paths: &[&Path],
    ) -> Result<Vec<(String, bool)>> {
        if !real_dir.exists() || !real_dir.is_dir() {
            return Ok(Vec::new());
        }

        let mut entries = Vec::new();
        for entry in std::fs::read_dir(real_dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };

            // Skip excluded paths (like mount points)
            if exclude_paths.iter().any(|exclude| path == *exclude) {
                debug!("Skipping excluded path: {path:?}");
                continue;
            }

            if path.is_dir() {
                entries.push((name.to_string(), true));
            } else if self.is_image_file(&path) {
                let display_name = self.get_display_name(&path, name);
                entries.push((display_name, false));
            }
        }
        Ok(entries)
    }

    fn get_display_name(&self, path: &Path, original_name: &str) -> String {
        // Fast extension-only check for directory listings
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if let Some(format) = ImageFormat::from_extension(ext) {
                if format.should_convert() {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        return format!("{stem}.heic");
                    }
                }
            }
        }
        original_name.to_string()
    }

    pub fn get_real_path(
        &self,
        virtual_path: &Path,
        source_paths: &[SourcePath],
    ) -> Option<PathBuf> {
        // Virtual path now starts with mount_name, e.g., "pictures/vacation/photo.heic"
        let mut components = virtual_path.components();
        let mount_name = components.next()?.as_os_str().to_str()?;
        let relative_path = components.as_path();

        log::trace!("get_real_path: mount_name={mount_name}, relative_path={relative_path:?}");

        // Find the source path that matches this mount name
        for source_path in source_paths {
            if source_path.mount_name == mount_name {
                let base_path = source_path.path.join(relative_path);
                log::trace!("get_real_path: base_path={base_path:?}");

                // If requesting a .heic file, try to find the original with any supported extension
                if virtual_path.extension().is_some_and(|ext| ext == "heic") {
                    let stem = base_path.file_stem()?;
                    let parent = base_path.parent()?;
                    log::trace!("get_real_path: searching for stem={stem:?} in parent={parent:?}");

                    // Check all supported extensions including heic (for recompression with new settings)
                    let extensions = ["heic", "jpg", "jpeg", "png", "gif", "webp", "bmp", "tiff"];
                    for ext in &extensions {
                        let real_path = parent.join(format!("{}.{}", stem.to_str()?, ext));
                        log::trace!("get_real_path: checking {real_path:?}");
                        if real_path.exists() {
                            // Only do expensive content check if extension suggests it's an image
                            if let Some(ext) = real_path.extension().and_then(|e| e.to_str()) {
                                if ImageFormat::from_extension(ext).is_some() {
                                    log::trace!("get_real_path: found source file for recompression {real_path:?}");
                                    return Some(real_path);
                                }
                            }
                        }
                    }
                    log::trace!("get_real_path: no matching file found for {virtual_path:?}");
                } else {
                    // Direct mapping for non-heic files
                    if base_path.exists() && self.is_image_file(&base_path) {
                        return Some(base_path);
                    }
                }

                // Only check the matching source path
                break;
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
        assert!(ImageFormat::Heic.should_convert()); // HEIC should recompress with new settings
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
