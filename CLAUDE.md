# FUSE Image Converter - Project Documentation

## Project Goals

**Problem**: Remote photo browsing consumes excessive bandwidth when downloading large images (7MB+) through apps like SolidExplorer.

**Solution**: A FUSE filesystem that transparently converts images to compressed HEIC format on-the-fly, reducing bandwidth usage by 70-90% without requiring client-side changes.

## Architecture Overview

### Core Components

- **`main.rs`**: CLI application with XDG-compliant paths (`~/.config`, `~/.cache`) and systemd --user service compatibility
- **`config.rs`**: YAML configuration system with quality settings, source paths, and cache management
- **`filesystem.rs`**: FUSE operations (lookup, getattr, read, readdir) creating virtual .heic files from source images
- **`image_converter.rs`**: Real HEIC encoding using libheif-rs with HEVC compression and configurable quality
- **`cache.rs`**: ATime-based LRU cache with disk persistence for converted images
- **`thread_pool.rs`**: Multi-threaded conversion utilizing all CPU cores
- **`file_detector.rs`**: Content-based image format detection with regex filename filtering

### Data Flow

1. **Discovery**: Scans configured source paths for images (`~/Pictures`, etc.)
2. **Virtual Mapping**: Presents images as `.heic` files in mount point
3. **On-Demand Conversion**: 
   - Cache check → Convert (if miss) → Cache result → Serve
   - Blocks read operations until conversion complete
4. **Caching**: Stores converted images with key format `{full_path}#{target_size}`

## Key Features

- **Transparent Operation**: Works with existing apps without modification
- **Bandwidth Optimization**: 70-90% compression vs original images
- **Performance**: Sub-millisecond cache hits, multi-threaded conversion
- **User Integration**: XDG Base Directory compliance, systemd service ready
- **Smart Caching**: Persistent across restarts, automatic LRU eviction

## Configuration

**Default Config**: `~/.config/fuse-img2heic-rs/config.yaml`
```yaml
source_paths:
  - path: "~/Pictures"
    recursive: true

heic_settings:
  quality: 50    # 1-100, lower = more compression
  speed: 4       # 1-10, higher = faster encoding
  chroma: 420    # 420/422/444

cache:
  max_size_mb: 1024
```

## Usage

```bash
# Initial setup
fuse-img2heic setup

# Run with default mount point from config
fuse-img2heic

# Override mount point
fuse-img2heic -m /mnt/images

# Run in foreground for debugging
fuse-img2heic -f

# Systemd user service
systemctl --user enable fuse-img2heic.service
```

## Technical Notes

- **Cache Location**: `~/.cache/fuse-img2heic-rs/`
- **Supported Formats**: JPEG, PNG, GIF, WebP, BMP, TIFF → HEIC
- **Dependencies**: libheif-rs for HEVC encoding, fuser for FUSE operations
- **Thread Safety**: DashMap for concurrent cache access, crossbeam channels for job distribution

This project solves remote bandwidth limitations while maintaining transparent file access through the standard filesystem interface.

## Development Memories

- Hard coding is bad, instead, add a configuration option in the YAML