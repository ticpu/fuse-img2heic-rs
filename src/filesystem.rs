use anyhow::Result;
use fuser::{
    FileAttr, FileType, Filesystem, KernelConfig, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
    Request,
};
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::cache::{create_cache_key_and_context_for_path, ImageCache};
use crate::config::Config;
use crate::file_detector::FileDetector;
use crate::image_converter;
use crate::thread_pool::ConversionThreadPool;

// TTL is now configured per instance via config
const ROOT_INODE: u64 = 1;

pub struct ImageFuseFS {
    config: Config,
    cache: Arc<ImageCache>,
    thread_pool: Arc<ConversionThreadPool>,
    file_detector: FileDetector,
    inode_map: HashMap<u64, PathBuf>, // inode -> virtual path
    path_map: HashMap<PathBuf, u64>,  // virtual path -> inode
    next_inode: u64,
    mount_point: PathBuf,
    ttl: Duration, // Cache TTL from config
}

impl ImageFuseFS {
    pub fn new(config: &Config, mount_point: PathBuf) -> Result<Self> {
        info!("Initializing ImageFuseFS");

        let cache_dir = config.get_cache_dir_from_config()?;
        let cache = ImageCache::new(
            config.cache.max_size_mb,
            cache_dir,
            config.cache.enable_encryption,
        )?;

        let num_workers = num_cpus::get();
        let thread_pool = Arc::new(ConversionThreadPool::new(num_workers, Arc::clone(&cache)));

        let file_detector = FileDetector::new(config.filename_patterns.clone())?;

        let ttl = Duration::from_secs(config.fuse.cache_timeout);
        let mut fs = Self {
            config: config.clone(),
            cache,
            thread_pool,
            file_detector,
            inode_map: HashMap::new(),
            path_map: HashMap::new(),
            next_inode: ROOT_INODE + 1,
            mount_point,
            ttl,
        };

        // Initialize root directory mapping
        fs.inode_map.insert(ROOT_INODE, PathBuf::from("/"));
        fs.path_map.insert(PathBuf::from("/"), ROOT_INODE);

        info!("ImageFuseFS initialized successfully");
        Ok(fs)
    }

    fn get_or_create_inode(&mut self, virtual_path: &Path) -> u64 {
        if let Some(&inode) = self.path_map.get(virtual_path) {
            return inode;
        }

        let inode = self.next_inode;
        self.next_inode += 1;

        self.inode_map.insert(inode, virtual_path.to_path_buf());
        self.path_map.insert(virtual_path.to_path_buf(), inode);

        log::trace!("Created inode {inode} for virtual path: {virtual_path:?}");
        inode
    }

    fn get_virtual_path(&self, inode: u64) -> Option<&PathBuf> {
        self.inode_map.get(&inode)
    }

    fn get_real_path(&self, virtual_path: &Path) -> Option<PathBuf> {
        self.file_detector
            .get_real_path(virtual_path, &self.config.source_paths)
    }

    fn is_virtual_directory(&self, virtual_path: &Path) -> bool {
        self.file_detector
            .is_virtual_directory(virtual_path, &self.config.source_paths)
    }

    /// Prefetch next N files in the same directory for faster sequential access
    fn prefetch_next_files(&self, current_real_path: &Path, count: usize) {
        let Some(parent) = current_real_path.parent() else {
            return;
        };
        let Some(current_name) = current_real_path.file_name() else {
            return;
        };

        // List files in directory and find current position
        let Ok(entries) = std::fs::read_dir(parent) else {
            return;
        };

        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && image_converter::is_convertible_format(p))
            .collect();
        files.sort();

        // Find current file and prefetch next N
        let current_idx = files.iter().position(|p| p.file_name() == Some(current_name));
        if let Some(idx) = current_idx {
            for path in files.iter().skip(idx + 1).take(count) {
                debug!("Prefetching: {path:?}");
                self.thread_pool.prefetch(path.clone(), self.config.heic_settings.clone());
            }
        }
    }

    fn preserve_original_timestamps(&self, attr: &mut FileAttr, real_path: &Path) {
        if let Ok(metadata) = std::fs::metadata(real_path) {
            if let Ok(mtime) = metadata.modified() {
                attr.mtime = mtime;
            }
            if let Ok(atime) = metadata.accessed() {
                attr.atime = atime;
            }
            if let Ok(ctime) = metadata.created() {
                attr.crtime = ctime;
            }
        }
    }

    fn create_file_attr(&self, size: u64, is_dir: bool) -> FileAttr {
        let now = SystemTime::now();

        FileAttr {
            ino: 0, // Will be set by caller
            size,
            blocks: size.div_ceil(512),
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: if is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if is_dir { 0o755 } else { 0o644 },
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            flags: 0,
            blksize: 4096,
        }
    }

    fn list_directory(&mut self, virtual_dir: &Path) -> Vec<(String, u64, FileType)> {
        log::trace!("Listing directory: {virtual_dir:?}");

        let mut entries = Vec::new();

        // Use lazy directory listing - only scans the specific directory requested
        // Exclude mount point to prevent infinite recursion
        if let Ok(dir_entries) = self.file_detector.list_virtual_directory_with_exclusions(
            virtual_dir,
            &self.config.source_paths,
            &[&self.mount_point],
        ) {
            for (name, is_directory) in dir_entries {
                let virtual_path = if virtual_dir == Path::new("/") {
                    PathBuf::from(&name)
                } else {
                    virtual_dir.join(&name)
                };

                let inode = self.get_or_create_inode(&virtual_path);
                let file_type = if is_directory {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                };

                entries.push((name, inode, file_type));
            }
        }

        log::trace!("Listed {} entries in {:?}", entries.len(), virtual_dir);
        entries
    }
}

impl Filesystem for ImageFuseFS {
    fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> Result<(), libc::c_int> {
        info!("FUSE filesystem initialized");
        Ok(())
    }

    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        log::trace!("lookup: parent={parent}, name={name:?}");

        let parent_path = match self.get_virtual_path(parent) {
            Some(path) => path.clone(),
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(libc::EINVAL);
                return;
            }
        };

        let virtual_path = if parent_path.as_os_str() == "/" {
            PathBuf::from(name_str)
        } else {
            parent_path.join(name_str)
        };

        log::trace!("Looking up virtual path: {virtual_path:?}");

        // Check if it's a real file
        log::trace!("Attempting to resolve real path for virtual: {virtual_path:?}");
        if let Some(real_path) = self.get_real_path(&virtual_path) {
            log::trace!("Found real path: {real_path:?}");
            let inode = self.get_or_create_inode(&virtual_path);

            // Check cache first for converted file size
            let original_size = std::fs::metadata(&real_path).map(|m| m.len()).unwrap_or(0);
            let (cache_key, context) = create_cache_key_and_context_for_path(
                &real_path,
                original_size,
                &self.config.heic_settings,
            );
            let size = if let Some(cached_data) = self.cache.get_with_context(&cache_key, &context)
            {
                // Use cached converted size
                cached_data.len() as u64
            } else {
                // Use original file size (no expensive conversion estimation)
                original_size
            };

            let mut attr = self.create_file_attr(size, false);
            attr.ino = inode;

            // Preserve original file timestamps
            self.preserve_original_timestamps(&mut attr, &real_path);

            reply.entry(&self.ttl, &attr, 0);
            return;
        } else {
            log::trace!("No real path found for virtual: {virtual_path:?}");
        }

        // Check if it's a directory (even empty ones)
        if self.is_virtual_directory(&virtual_path) {
            let inode = self.get_or_create_inode(&virtual_path);
            let mut attr = self.create_file_attr(0, true);
            attr.ino = inode;

            reply.entry(&self.ttl, &attr, 0);
            return;
        }

        reply.error(libc::ENOENT);
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        log::trace!("getattr: ino={ino}");

        if ino == ROOT_INODE {
            let mut attr = self.create_file_attr(0, true);
            attr.ino = ROOT_INODE;
            reply.attr(&self.ttl, &attr);
            return;
        }

        let virtual_path = match self.get_virtual_path(ino) {
            Some(path) => path.clone(),
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Check if it's a file
        if let Some(real_path) = self.get_real_path(&virtual_path) {
            // Check cache first for converted file size (same logic as lookup)
            let original_size = std::fs::metadata(&real_path).map(|m| m.len()).unwrap_or(0);
            let (cache_key, context) = create_cache_key_and_context_for_path(
                &real_path,
                original_size,
                &self.config.heic_settings,
            );
            // Use cached size if available, otherwise original size
            // (kernel handles short reads at EOF fine)
            let size = if let Some(cached_data) = self.cache.get_with_context(&cache_key, &context)
            {
                cached_data.len() as u64
            } else {
                original_size
            };

            let mut attr = self.create_file_attr(size, false);
            attr.ino = ino;

            // Preserve original file timestamps
            self.preserve_original_timestamps(&mut attr, &real_path);

            reply.attr(&self.ttl, &attr);
            return;
        }

        // Check if it's a directory (even empty ones)
        if self.is_virtual_directory(&virtual_path) {
            let mut attr = self.create_file_attr(0, true);
            attr.ino = ino;
            reply.attr(&self.ttl, &attr);
            return;
        }

        reply.error(libc::ENOENT);
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        log::trace!("read: ino={ino}, offset={offset}, size={size}");

        let virtual_path = match self.get_virtual_path(ino) {
            Some(path) => path.clone(),
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let real_path = match self.get_real_path(&virtual_path) {
            Some(path) => path,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        // Prefetch next files in directory for faster sequential access
        if self.config.fuse.prefetch_count > 0 {
            self.prefetch_next_files(&real_path, self.config.fuse.prefetch_count);
        }

        // Create cache key
        let original_size = std::fs::metadata(&real_path).map(|m| m.len()).unwrap_or(0);
        let (cache_key, context) = create_cache_key_and_context_for_path(
            &real_path,
            original_size,
            &self.config.heic_settings,
        );

        // Try cache first
        if let Some(cached_data) = self.cache.get_with_context(&cache_key, &context) {
            log::trace!("Serving from cache: {real_path:?}");
            let end = std::cmp::min(offset as usize + size as usize, cached_data.len());
            let start = std::cmp::min(offset as usize, cached_data.len());
            log::trace!(
                "Serving cached bytes {start}-{end} of {} total",
                cached_data.len()
            );
            reply.data(&cached_data[start..end]);
            return;
        }

        // Convert if needed
        let is_convertible = image_converter::is_convertible_format(&real_path);
        log::trace!("is_convertible_format({real_path:?}) = {is_convertible}");
        let data = if is_convertible {
            debug!("Converting image: {real_path:?}");
            match self
                .thread_pool
                .convert_image_blocking(real_path.clone(), self.config.heic_settings.clone())
            {
                Ok(converted_data) => {
                    debug!(
                        "Conversion successful, {} bytes, caching result",
                        converted_data.len()
                    );
                    if let Err(e) =
                        self.cache
                            .put_with_context(cache_key, converted_data.clone(), &context)
                    {
                        warn!("Failed to cache converted image: {e}");
                    }
                    converted_data
                }
                Err(e) => {
                    error!("Conversion failed for {real_path:?}: {e}");
                    // Fallback to original file
                    match std::fs::read(&real_path) {
                        Ok(original_data) => {
                            debug!("Using original file, {} bytes", original_data.len());
                            if let Err(e) = self.cache.put_with_context(
                                cache_key.clone(),
                                original_data.clone(),
                                &context,
                            ) {
                                warn!("Failed to cache original image: {e}");
                            }
                            original_data
                        }
                        Err(e) => {
                            error!("Failed to read original file {real_path:?}: {e}");
                            reply.error(libc::EIO);
                            return;
                        }
                    }
                }
            }
        } else {
            // Serve original file
            match std::fs::read(&real_path) {
                Ok(original_data) => {
                    if let Err(e) =
                        self.cache
                            .put_with_context(cache_key, original_data.clone(), &context)
                    {
                        warn!("Failed to cache original file: {e}");
                    }
                    original_data
                }
                Err(e) => {
                    error!("Failed to read file {real_path:?}: {e}");
                    reply.error(libc::EIO);
                    return;
                }
            }
        };

        // Return requested portion
        let end = std::cmp::min(offset as usize + size as usize, data.len());
        let start = std::cmp::min(offset as usize, data.len());
        log::trace!("Serving bytes {start}-{end} of {} total", data.len());
        reply.data(&data[start..end]);
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        log::trace!("readdir: ino={ino}, offset={offset}");

        let virtual_path = match self.get_virtual_path(ino) {
            Some(path) => path.clone(),
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        let entries = self.list_directory(&virtual_path);

        let mut index = 0i64;

        // Add . and .. entries
        if index >= offset && reply.add(ino, index + 1, FileType::Directory, ".") {
            reply.ok();
            return;
        }
        index += 1;

        if virtual_path != Path::new("/") {
            if index >= offset {
                let parent_inode = if let Some(parent) = virtual_path.parent() {
                    self.get_or_create_inode(parent)
                } else {
                    ROOT_INODE
                };
                if reply.add(parent_inode, index + 1, FileType::Directory, "..") {
                    reply.ok();
                    return;
                }
            }
            index += 1;
        }

        // Add discovered entries
        for (name, entry_inode, file_type) in entries {
            if index >= offset && reply.add(entry_inode, index + 1, file_type, &name) {
                break;
            }
            index += 1;
        }

        reply.ok();
    }

    fn open(&mut self, _req: &Request, ino: u64, _flags: i32, reply: fuser::ReplyOpen) {
        log::trace!("open: ino={ino}");

        let virtual_path = match self.get_virtual_path(ino) {
            Some(path) => path.clone(),
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        if self.get_real_path(&virtual_path).is_some() {
            reply.opened(0, 0);
        } else {
            reply.error(libc::ENOENT);
        }
    }
}
