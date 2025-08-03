# FUSE Image to HEIC Converter

A FUSE filesystem that transparently converts images to compressed HEIC format on-the-fly, designed specifically for **remote photo browsing bandwidth optimization**.

## The Problem

When browsing photos remotely through mobile apps like **SolidExplorer**, large images (7MB+ PNG/JPEG files) waste enormous bandwidth. These apps download the **entire 7MB file just to generate a thumbnail**, making remote photo browsing painfully slow and data-expensive over mobile connections.

## The Solution

This FUSE filesystem creates a virtual mount point where:
- **All images appear as `.heic` files** in directory listings
- **Conversion happens on-demand** when files are actually read
- **Massive compression** reduces 7MB images to ~200-500KB (90-95% savings)
- **Works transparently** with any app - no client-side changes needed
- **Smart caching** means subsequent access is instant

## Real-World Impact

| Scenario | Before | After | Savings |
|----------|---------|--------|---------|
| **Thumbnail browsing** | 7MB PNG download | 300KB HEIC download | **95% less data** |
| **Photo gallery app** | 100MB for 15 photos | 8MB for 15 photos | **92% less bandwidth** |
| **Mobile hotspot** | Exhausts data plan | Browse freely | **10x more photos** |

## Key Features

### 🚀 **Transparent Operation**
- Works with existing apps (file browsers, photo viewers, backup tools)
- No app modifications required
- Standard filesystem interface

### 📱 **Remote Access Optimized**
- Designed for mobile/slow connections
- On-demand conversion (no eager processing)
- Optimized for thumbnail generation workflows

### 🎯 **Smart Compression**
- **HEIC with HEVC encoding** for maximum compression
- **Configurable quality** settings (1-100)
- **Resolution limiting** (e.g., auto-resize to 1440p)
- **90-95% typical compression** with good visual quality

### ⚡ **High Performance**
- **Multi-threaded conversion** using all CPU cores
- **SHA256-based cache** with privacy-preserving file names
- **Sub-millisecond cache hits** for repeated access
- **Lazy directory listing** (no upfront scanning)

### 📂 **Format Support**
- **Input**: JPEG, PNG, GIF, WebP, BMP, TIFF, HEIC
- **Output**: Compressed HEIC with HEVC
- **Content-based detection** (not just file extensions)
- **HEIC-to-HEIC recompression** with new quality settings

## Installation

### Prerequisites
```bash
# Ubuntu/Debian
sudo apt install libheif-dev libfuse3-dev build-essential

# CentOS/RHEL
sudo yum install libheif-devel fuse3-devel gcc

# macOS
brew install libheif
```

### Build & Install
```bash
git clone https://github.com/your-repo/fuse-img2heic-rs
cd fuse-img2heic-rs
cargo build --release
sudo cp target/release/fuse-img2heic-rs /usr/local/bin/
```

## Quick Start

```bash
# 1. Create initial setup
fuse-img2heic-rs setup

# 2. Edit configuration for your photo directories
vim ~/.config/fuse-img2heic-rs/config.yaml

# 3. Mount the filesystem
fuse-img2heic-rs

# 4. Access compressed photos
ls /tmp/fuse-img2heic/pictures/  # See .heic versions of your photos

# 5. Unmount when done
fusermount3 -u /tmp/fuse-img2heic
```

## Configuration

Location: `~/.config/fuse-img2heic-rs/config.yaml`

```yaml
# Virtual mount point
mount_point: "/tmp/fuse-img2heic"

# Source photo directories
source_paths:
  - path: "~/Pictures"           # Your main photo collection
    recursive: true              # Include subdirectories
    mount_name: "pictures"       # Appears as /mount/pictures/

  - path: "~/DCIM"              # Camera photos
    recursive: true
    mount_name: "camera"

  - path: "~/Downloads"          # Recent downloads only
    recursive: false             # No subdirectories
    mount_name: "downloads"

# Image file detection (regex patterns)
filename_patterns:
  - ".*\\.(jpg|jpeg|png|gif|heic|webp|bmp|tiff)$"

# HEIC compression settings
heic_settings:
  quality: 30                    # 1-100 (30 = high compression, good quality)
  speed: 4                       # 1-10 (encoding speed vs efficiency)
  chroma: 420                    # Color subsampling (420/422/444)
  max_resolution: "2560,1440"   # Auto-resize to 1440p (optional)

# Performance tuning
cache:
  max_size_mb: 2048             # Cache up to 2GB of converted images

fuse:
  cache_timeout: 300            # Cache filesystem operations for 5 minutes

# Logging (use -vv for debug during setup)
logging:
  level: "warn"                 # warn/info/debug/trace
```

## Usage Scenarios

### 📱 **Mobile Remote Access**
```bash
# Mount your photos with high compression
fuse-img2heic-rs setup
# Edit config: quality: 20 for maximum compression
fuse-img2heic-rs

# Now browse /tmp/fuse-img2heic/ from SolidExplorer
# 7MB photos become 200KB downloads automatically
```

## Command Line Reference

```bash
fuse-img2heic-rs [OPTIONS] [mount-point]

Commands:
  setup                    Create config directories and default config

Options:
  -m, --mount <PATH>      Override mount point from config
  -c, --config <PATH>     Use custom config file
  -f, --foreground        Run in foreground (for debugging)
  -v                      Info logging (-v)
  -vv                     Debug logging (-vv)
  -vvv                    Trace logging (-vvv)
  -h, --help             Show help

Examples:
  fuse-img2heic-rs                    # Use config mount point
  fuse-img2heic-rs /mnt/photos        # Override mount point
  fuse-img2heic-rs -vv -f             # Debug mode, foreground
```

## Technical Architecture

### Virtual File System
```
Source: ~/Pictures/vacation.jpg (7MB)
   ↓
Virtual: /mount/pictures/vacation.heic (300KB)
   ↓
Cache: ~/.cache/fuse-img2heic-rs/a1/b2c3d4... (privacy-preserving)
```

### Cache Strategy
- **SHA256 keys**: `hash(filepath + filesize)` prevents path leaks
- **Directory structure**: `.cache/xx/xxxxx` for efficient storage
- **LRU eviction**: Automatic cleanup of old conversions
- **Persistent**: Survives restarts

### Performance Characteristics
- **First access**: 1-3 seconds (conversion + caching)
- **Subsequent access**: <1ms (cache hit)
- **Compression ratio**: 70-95% typical reduction
- **CPU usage**: Multi-core conversion, minimal overhead
- **Memory**: Configurable cache size, streaming I/O

## Troubleshooting

### Common Issues

**"Permission denied" on mount**:
```bash
sudo usermod -a -G fuse $USER
# Log out and log back in
```

**"No images visible"**:
```bash
# Check config paths and run with debug
fuse-img2heic-rs -vv -f
```

**"Images are corrupted"**:
```bash
# Check if libheif is properly installed
ldconfig -p | grep heif
```

**"Too slow conversion"**:
```bash
# Increase speed setting in config
heic_settings:
  speed: 8  # Faster encoding
```

### Debug Mode
```bash
# Run with full logging to see what's happening
fuse-img2heic-rs -vvv -f

# Monitor actual file operations
strace -e trace=openat,read,write fuse-img2heic-rs -f
```

## Development

Built with Rust for **safety**, **performance**, and **reliability**.

### Key Dependencies
- `fuser` - FUSE bindings
- `libheif-rs` - HEIC encoding/decoding
- `image` - Image format handling
- `serde_yaml` - Configuration
- `dashmap` - Concurrent caching
- `crossbeam` - Multi-threading

### Architecture
- **Lazy evaluation**: Only convert on actual file reads
- **Thread pool**: Multi-core conversion pipeline
- **Zero-copy I/O**: Streaming data handling
- **Privacy-first**: No filepath exposure in cache

## Related Projects & Keywords

**FUSE image processing**, **image compression filesystem**, **bandwidth optimization**, **remote photo access**, **HEIC conversion**, **thumbnail optimization**, **mobile data saving**, **transparent image proxy**, **on-demand compression**, **photo gallery optimization**
