# FUSE Image to HEIC Converter - Architecture Documentation

## System Architecture

### Core Components

**`main.rs`** - CLI entry point with XDG-compliant paths and systemd compatibility
**`config.rs`** - YAML configuration system with structured settings
**`filesystem.rs`** - FUSE operations implementation (lookup, getattr, read, readdir)
**`image_converter.rs`** - HEIC encoding/decoding using libheif-rs
**`cache.rs`** - SHA256-based LRU cache with disk persistence
**`thread_pool.rs`** - Multi-threaded conversion pipeline
**`file_detector.rs`** - Content-based image format detection and virtual path mapping
**`mount_management.rs`** - Mount point management and signal handling

### Data Flow

```
Directory Listing Request
├─ file_detector.rs: list_virtual_directory_with_exclusions()
├─ Virtual path mapping with mount_name organization
└─ Return .heic filenames for all images

File Read Request
├─ filesystem.rs: lookup() → get_real_path()
├─ cache.rs: Check SHA256(filepath + filesize) key
├─ Cache miss: thread_pool.rs → image_converter.rs
├─ HEIC conversion with quality/resolution settings
├─ cache.rs: Store in .cache/xx/xxxxx structure
└─ Return converted data
```

### Virtual Filesystem Structure

Source paths are mapped to virtual hierarchy via `mount_name`:
```
Real: ~/Pictures/vacation.jpg + ~/DCIM/photo.png
Virtual: /mount/pictures/vacation.heic + /mount/camera/photo.heic
```

## Code Organization

### Configuration Structure
```rust
struct Config {
    source_paths: Vec<SourcePath>,  // Real directories to scan
    heic_settings: HeicSettings,    // Compression parameters
    cache: CacheSettings,           // Size limits and paths
    fuse: FuseSettings,             // Filesystem caching
}
```

### FUSE Implementation Pattern
- **Lazy evaluation**: No eager directory scanning
- **Inode management**: HashMap-based virtual path tracking
- **Error handling**: Proper FUSE error codes (ENOENT, EINVAL, EIO)

### Cache Architecture
```rust
Key: SHA256(filepath + original_filesize)
Storage: ~/.cache/fuse-img2heic-rs/{first_2_hex_chars}/{remaining_hex}
Eviction: ATime-based LRU with configurable size limits
```

### Thread Safety
- `DashMap` for concurrent cache access
- `crossbeam` channels for job distribution
- Thread pool blocks on conversion completion
- No shared mutable state in FUSE operations

## Critical Implementation Notes

### HEIC Decoding Requirements
- libheif-rs API requires specific color space handling
- HEIC-to-HEIC recompression needs `decode_heic_with_libheif()`
- Interleaved RGB plane extraction with stride handling
- Must handle both existing HEIC and other formats

### Path Resolution Logic
```rust
// Virtual path: "mount_name/subpath/file.heic" 
// Real path: source_path.path/subpath/file.{jpg,png,heic,...}
```

### Mount Point Exclusion
Critical: Exclude mount point from directory listings to prevent infinite recursion when source path contains mount point.

### Error Handling Strategy
- Fallback to original file if conversion fails
- Cache original data on conversion errors
- Log conversion failures but continue serving

## Dependencies and Requirements

### System Dependencies
- `libheif-dev` - HEIC encoding/decoding library
- `libfuse3-dev` - FUSE filesystem interface

### Rust Dependencies
- `fuser` - FUSE bindings for Rust
- `libheif-rs` - Safe libheif wrapper
- `image` - Image format handling and basic conversions
- `serde_yaml` - Configuration file parsing
- `dashmap` - Concurrent HashMap
- `crossbeam` - Threading primitives
- `sha2` + `hex` - Cache key generation
- `anyhow` - Error handling
- `clap` - CLI argument parsing

## Coding Practices

### Logging Levels
- `trace`: FUSE operations, cache hits/misses, path resolution
- `debug`: Image conversion start/completion, file discoveries
- `info`: Mount/unmount, cache loading, worker thread status
- `warn`: Configuration issues, fallback behaviors

### Configuration Pattern
All hardcoded values should be moved to YAML configuration with sensible defaults.

### Testing Strategy
Unit tests for format detection, cache key generation, and configuration parsing.
Integration tests require actual image files and temporary directories.

### Memory Management
Streaming I/O for large images, configurable cache sizes, explicit cleanup on unmount.