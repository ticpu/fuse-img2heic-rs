# FUSE Image Converter

A FUSE filesystem that transparently converts images to compressed HEIC format on-the-fly for efficient remote browsing.

## Problem

When browsing photos remotely (e.g., using SolidExplorer on your phone), downloading large images (sometimes 7MB+) consumes significant bandwidth. This FUSE filesystem solves this by transparently converting images to highly compressed HEIC format, reducing bandwidth usage by 70-90%.

## Features

- **Transparent Conversion**: Images appear as `.heic` files with significantly reduced sizes
- **Multi-threaded**: Utilizes all CPU cores for fast conversion
- **Smart Caching**: LRU cache with configurable size and disk persistence
- **Content Detection**: Detects image formats by content, not just file extension
- **XDG Compliant**: Uses standard user directories (`~/.config`, `~/.cache`)
- **Systemd Ready**: Designed to run as a user service

## Installation

### Prerequisites

- Rust (latest stable)
- FUSE development libraries
- libheif development libraries

On Ubuntu/Debian:
```bash
sudo apt install build-essential libfuse-dev libheif-dev
```

On Arch Linux:
```bash
sudo pacman -S base-devel fuse2 libheif
```

### Build

```bash
git clone <repository>
cd fuse-img2heic-rs
cargo build --release
```

## Configuration

The application uses `~/.config/fuse-img2heic-rs/config.yaml` by default. A sample configuration:

```yaml
mount_point: "/tmp/fuse-img2heic"

source_paths:
  - path: "~/Pictures"
    recursive: true
    mount_name: "pictures"     # Appears as pictures/ in mount
  - path: "~/Downloads"
    recursive: false
    mount_name: "downloads"    # Appears as downloads/ in mount

filename_patterns:
  - ".*\\.(jpg|jpeg|png|gif|heic|webp|bmp|tiff)$"

heic_settings:
  quality: 50          # 1-100, lower = more compression
  speed: 4             # 1-10, higher = faster encoding
  chroma: 420          # 420, 422, 444

cache:
  max_size_mb: 1024    # LRU eviction based on access time

logging:
  level: "info"
```

## Usage

### Manual Run

```bash
# Run in foreground
./target/release/fuse-img2heic-rs -m /mnt/images -f

# Run in background
./target/release/fuse-img2heic-rs -m /mnt/images

# Custom config
./target/release/fuse-img2heic-rs -c /path/to/config.yaml -m /mnt/images
```

### Systemd User Service

Create `~/.config/systemd/user/fuse-img2heic.service`:

```ini
[Unit]
Description=FUSE Image Converter
After=graphical-session.target

[Service]
Type=simple
ExecStart=/path/to/fuse-img2heic-rs --mount /home/%i/mnt/images
ExecStop=/bin/fusermount -u /home/%i/mnt/images
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

Enable and start:
```bash
systemctl --user daemon-reload
systemctl --user enable fuse-img2heic.service
systemctl --user start fuse-img2heic.service
```

## How It Works

1. **File Discovery**: Scans configured source paths for images
2. **Virtual Filesystem**: Creates a virtual directory structure where images appear as `.heic` files
3. **On-Demand Conversion**: When a file is accessed:
   - Checks cache first
   - If miss, converts image to HEIC using background thread pool
   - Caches result for future access
   - Returns converted data

## Performance

- **Cache Hit**: Sub-millisecond response time
- **Cache Miss**: Conversion time varies by image size and CPU
- **Memory Usage**: Configurable cache size (default 1GB)
- **CPU Usage**: Utilizes all available cores for conversion
- **Compression**: Typically 70-90% size reduction vs original

## Cache Management

- **Location**: `~/.cache/fuse-img2heic-rs/`
- **Strategy**: LRU eviction based on access time
- **Persistence**: Cache survives across restarts
- **Key Format**: `{full_path}#{target_size}`

## Troubleshooting

### Mount Issues
```bash
# Check if mount point is busy
fusermount -u /mnt/images

# Check FUSE permissions
ls -l /dev/fuse
```

### Performance Issues
```bash
# Check cache usage
ls -la ~/.cache/fuse-img2heic-rs/

# Monitor logs
journalctl --user -u fuse-img2heic.service -f
```

### Configuration Issues
```bash
# Validate config
fuse-img2heic-rs --help

# Check source paths exist
ls -la ~/Pictures
```

## Bandwidth Savings Example

| Format | Original Size | HEIC Size | Savings |
|--------|---------------|-----------|---------|
| JPEG (High Quality) | 7.2 MB | 1.8 MB | 75% |
| PNG (Screenshot) | 4.5 MB | 0.9 MB | 80% |
| RAW/TIFF | 25 MB | 3.2 MB | 87% |

## License

MIT License - see LICENSE file for details.