use anyhow::Result;
use bytes::Bytes;
use dashmap::DashMap;
use fuse3::raw::prelude::*;
use fuse3::{Errno, FileType, Inode, Timestamp};
use futures_util::stream::{self, BoxStream};
use log::{debug, error, info, warn};
use std::ffi::OsStr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::cache::{create_cache_key_and_context_for_path, ImageCache};
use crate::config::Config;
use crate::file_detector::FileDetector;
use crate::image_converter;
use crate::thread_pool::ConversionThreadPool;

const ROOT_INODE: u64 = 1;

pub struct ImageFuseFS {
    config: Config,
    cache: Arc<ImageCache>,
    thread_pool: Arc<ConversionThreadPool>,
    file_detector: FileDetector,
    inode_map: DashMap<u64, PathBuf>,
    path_map: DashMap<PathBuf, u64>,
    next_inode: AtomicU64,
    mount_point: PathBuf,
    ttl: Duration,
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
        let inode_map = DashMap::new();
        let path_map = DashMap::new();

        inode_map.insert(ROOT_INODE, PathBuf::from("/"));
        path_map.insert(PathBuf::from("/"), ROOT_INODE);

        let fs = Self {
            config: config.clone(),
            cache,
            thread_pool,
            file_detector,
            inode_map,
            path_map,
            next_inode: AtomicU64::new(ROOT_INODE + 1),
            mount_point,
            ttl,
        };

        info!("ImageFuseFS initialized successfully");
        Ok(fs)
    }

    fn get_or_create_inode(&self, virtual_path: &Path) -> u64 {
        if let Some(inode) = self.path_map.get(virtual_path) {
            return *inode;
        }

        let inode = self.next_inode.fetch_add(1, Ordering::SeqCst);

        self.inode_map.insert(inode, virtual_path.to_path_buf());
        self.path_map.insert(virtual_path.to_path_buf(), inode);

        log::trace!("Created inode {inode} for virtual path: {virtual_path:?}");
        inode
    }

    fn get_virtual_path(&self, inode: u64) -> Option<PathBuf> {
        self.inode_map.get(&inode).map(|r| r.clone())
    }

    fn get_real_path(&self, virtual_path: &Path) -> Option<PathBuf> {
        self.file_detector
            .get_real_path(virtual_path, &self.config.source_paths)
    }

    fn is_virtual_directory(&self, virtual_path: &Path) -> bool {
        self.file_detector
            .is_virtual_directory(virtual_path, &self.config.source_paths)
    }

    fn prefetch_next_files(&self, current_real_path: &Path, count: usize) {
        let Some(parent) = current_real_path.parent() else {
            return;
        };
        let Some(current_name) = current_real_path.file_name() else {
            return;
        };

        let Ok(entries) = std::fs::read_dir(parent) else {
            return;
        };

        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && image_converter::is_convertible_format(p))
            .collect();
        files.sort();

        let current_idx = files.iter().position(|p| p.file_name() == Some(current_name));
        if let Some(idx) = current_idx {
            for path in files.iter().skip(idx + 1).take(count) {
                debug!("Prefetching: {path:?}");
                self.thread_pool
                    .prefetch(path.clone(), self.config.heic_settings.clone());
            }
        }
    }

    fn system_time_to_timestamp(st: SystemTime) -> Timestamp {
        let duration = st.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
        Timestamp::new(duration.as_secs() as i64, duration.subsec_nanos())
    }

    fn create_file_attr(&self, ino: u64, size: u64, is_dir: bool) -> FileAttr {
        let now = Self::system_time_to_timestamp(SystemTime::now());

        FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: now,
            mtime: now,
            ctime: now,
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
            blksize: 4096,
        }
    }

    fn preserve_original_timestamps(&self, attr: &mut FileAttr, real_path: &Path) {
        if let Ok(metadata) = std::fs::metadata(real_path) {
            if let Ok(mtime) = metadata.modified() {
                attr.mtime = Self::system_time_to_timestamp(mtime);
            }
            if let Ok(atime) = metadata.accessed() {
                attr.atime = Self::system_time_to_timestamp(atime);
            }
        }
    }

    fn list_directory(&self, virtual_dir: &Path) -> Vec<(String, u64, FileType)> {
        log::trace!("Listing directory: {virtual_dir:?}");

        let mut entries = Vec::new();

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
    type DirEntryStream<'a> = BoxStream<'a, fuse3::Result<DirectoryEntry>>;
    type DirEntryPlusStream<'a> = BoxStream<'a, fuse3::Result<DirectoryEntryPlus>>;

    async fn init(&self, _req: Request) -> fuse3::Result<ReplyInit> {
        info!("FUSE filesystem initialized");
        Ok(ReplyInit {
            max_write: NonZeroU32::new(1024 * 1024).unwrap(),
        })
    }

    async fn destroy(&self, _req: Request) {
        info!("FUSE filesystem destroyed");
    }

    async fn lookup(&self, _req: Request, parent: Inode, name: &OsStr) -> fuse3::Result<ReplyEntry> {
        log::trace!("lookup: parent={parent}, name={name:?}");

        let parent_path = self
            .get_virtual_path(parent)
            .ok_or(Errno::from(libc::ENOENT))?;

        let name_str = name.to_str().ok_or(Errno::from(libc::EINVAL))?;

        let virtual_path = if parent_path.as_os_str() == "/" {
            PathBuf::from(name_str)
        } else {
            parent_path.join(name_str)
        };

        log::trace!("Looking up virtual path: {virtual_path:?}");

        if let Some(real_path) = self.get_real_path(&virtual_path) {
            log::trace!("Found real path: {real_path:?}");
            let inode = self.get_or_create_inode(&virtual_path);

            let original_size = std::fs::metadata(&real_path).map(|m| m.len()).unwrap_or(0);
            let (cache_key, context) = create_cache_key_and_context_for_path(
                &real_path,
                original_size,
                &self.config.heic_settings,
            );
            let size = if let Some(cached_data) = self.cache.get_with_context(&cache_key, &context)
            {
                cached_data.len() as u64
            } else {
                original_size
            };

            let mut attr = self.create_file_attr(inode, size, false);
            self.preserve_original_timestamps(&mut attr, &real_path);

            return Ok(ReplyEntry {
                ttl: self.ttl,
                attr,
                generation: 0,
            });
        }

        if self.is_virtual_directory(&virtual_path) {
            let inode = self.get_or_create_inode(&virtual_path);
            let attr = self.create_file_attr(inode, 0, true);

            return Ok(ReplyEntry {
                ttl: self.ttl,
                attr,
                generation: 0,
            });
        }

        Err(Errno::from(libc::ENOENT))
    }

    async fn getattr(
        &self,
        _req: Request,
        inode: Inode,
        _fh: Option<u64>,
        _flags: u32,
    ) -> fuse3::Result<ReplyAttr> {
        log::trace!("getattr: ino={inode}");

        if inode == ROOT_INODE {
            let attr = self.create_file_attr(ROOT_INODE, 0, true);
            return Ok(ReplyAttr {
                ttl: self.ttl,
                attr,
            });
        }

        let virtual_path = self
            .get_virtual_path(inode)
            .ok_or(Errno::from(libc::ENOENT))?;

        if let Some(real_path) = self.get_real_path(&virtual_path) {
            let original_size = std::fs::metadata(&real_path).map(|m| m.len()).unwrap_or(0);
            let (cache_key, context) = create_cache_key_and_context_for_path(
                &real_path,
                original_size,
                &self.config.heic_settings,
            );
            let size = if let Some(cached_data) = self.cache.get_with_context(&cache_key, &context)
            {
                cached_data.len() as u64
            } else {
                original_size
            };

            let mut attr = self.create_file_attr(inode, size, false);
            self.preserve_original_timestamps(&mut attr, &real_path);

            return Ok(ReplyAttr {
                ttl: self.ttl,
                attr,
            });
        }

        if self.is_virtual_directory(&virtual_path) {
            let attr = self.create_file_attr(inode, 0, true);
            return Ok(ReplyAttr {
                ttl: self.ttl,
                attr,
            });
        }

        Err(Errno::from(libc::ENOENT))
    }

    async fn read(
        &self,
        _req: Request,
        inode: Inode,
        _fh: u64,
        offset: u64,
        size: u32,
    ) -> fuse3::Result<ReplyData> {
        log::trace!("read: ino={inode}, offset={offset}, size={size}");

        let virtual_path = self
            .get_virtual_path(inode)
            .ok_or(Errno::from(libc::ENOENT))?;

        let real_path = self
            .get_real_path(&virtual_path)
            .ok_or(Errno::from(libc::ENOENT))?;

        if self.config.fuse.prefetch_count > 0 {
            self.prefetch_next_files(&real_path, self.config.fuse.prefetch_count);
        }

        let original_size = std::fs::metadata(&real_path).map(|m| m.len()).unwrap_or(0);
        let (cache_key, context) = create_cache_key_and_context_for_path(
            &real_path,
            original_size,
            &self.config.heic_settings,
        );

        if let Some(cached_data) = self.cache.get_with_context(&cache_key, &context) {
            log::trace!("Serving from cache: {real_path:?}");
            let end = std::cmp::min(offset as usize + size as usize, cached_data.len());
            let start = std::cmp::min(offset as usize, cached_data.len());
            log::trace!(
                "Serving cached bytes {start}-{end} of {} total",
                cached_data.len()
            );
            return Ok(ReplyData {
                data: Bytes::copy_from_slice(&cached_data[start..end]),
            });
        }

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
                    return Err(Errno::from(libc::EIO));
                }
            }
        } else {
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
                    return Err(Errno::from(libc::EIO));
                }
            }
        };

        let end = std::cmp::min(offset as usize + size as usize, data.len());
        let start = std::cmp::min(offset as usize, data.len());
        log::trace!("Serving bytes {start}-{end} of {} total", data.len());

        Ok(ReplyData {
            data: Bytes::copy_from_slice(&data[start..end]),
        })
    }

    async fn open(&self, _req: Request, inode: Inode, _flags: u32) -> fuse3::Result<ReplyOpen> {
        log::trace!("open: ino={inode}");

        let virtual_path = self
            .get_virtual_path(inode)
            .ok_or(Errno::from(libc::ENOENT))?;

        if self.get_real_path(&virtual_path).is_some() {
            Ok(ReplyOpen { fh: 0, flags: 0 })
        } else {
            Err(Errno::from(libc::ENOENT))
        }
    }

    async fn opendir(&self, _req: Request, inode: Inode, _flags: u32) -> fuse3::Result<ReplyOpen> {
        log::trace!("opendir: ino={inode}");

        if inode == ROOT_INODE {
            return Ok(ReplyOpen { fh: 0, flags: 0 });
        }

        let virtual_path = self
            .get_virtual_path(inode)
            .ok_or(Errno::from(libc::ENOENT))?;

        if self.is_virtual_directory(&virtual_path) {
            Ok(ReplyOpen { fh: 0, flags: 0 })
        } else {
            Err(Errno::from(libc::ENOTDIR))
        }
    }

    async fn readdir<'a>(
        &'a self,
        _req: Request,
        parent: Inode,
        _fh: u64,
        offset: i64,
    ) -> fuse3::Result<ReplyDirectory<Self::DirEntryStream<'a>>> {
        log::trace!("readdir: ino={parent}, offset={offset}");

        let virtual_path = self
            .get_virtual_path(parent)
            .ok_or(Errno::from(libc::ENOENT))?;

        let entries = self.list_directory(&virtual_path);

        let mut all_entries: Vec<fuse3::Result<DirectoryEntry>> = Vec::new();
        let mut index = 0i64;

        all_entries.push(Ok(DirectoryEntry {
            inode: parent,
            kind: FileType::Directory,
            name: ".".into(),
            offset: index + 1,
        }));
        index += 1;

        if virtual_path != Path::new("/") {
            let parent_inode = if let Some(parent_dir) = virtual_path.parent() {
                self.get_or_create_inode(parent_dir)
            } else {
                ROOT_INODE
            };
            all_entries.push(Ok(DirectoryEntry {
                inode: parent_inode,
                kind: FileType::Directory,
                name: "..".into(),
                offset: index + 1,
            }));
            index += 1;
        }

        for (name, entry_inode, file_type) in entries {
            all_entries.push(Ok(DirectoryEntry {
                inode: entry_inode,
                kind: file_type,
                name: name.into(),
                offset: index + 1,
            }));
            index += 1;
        }

        let stream = stream::iter(all_entries.into_iter().skip(offset as usize));

        Ok(ReplyDirectory {
            entries: Box::pin(stream),
        })
    }

    async fn readdirplus<'a>(
        &'a self,
        _req: Request,
        parent: Inode,
        _fh: u64,
        offset: u64,
        _lock_owner: u64,
    ) -> fuse3::Result<ReplyDirectoryPlus<Self::DirEntryPlusStream<'a>>> {
        log::trace!("readdirplus: ino={parent}, offset={offset}");

        let virtual_path = self
            .get_virtual_path(parent)
            .ok_or(Errno::from(libc::ENOENT))?;

        let entries = self.list_directory(&virtual_path);

        let mut all_entries: Vec<fuse3::Result<DirectoryEntryPlus>> = Vec::new();
        let mut index = 0u64;

        // Add "."
        let dot_attr = self.create_file_attr(parent, 0, true);
        all_entries.push(Ok(DirectoryEntryPlus {
            inode: parent,
            generation: 0,
            kind: FileType::Directory,
            name: ".".into(),
            offset: (index + 1) as i64,
            attr: dot_attr,
            entry_ttl: self.ttl,
            attr_ttl: self.ttl,
        }));
        index += 1;

        // Add ".."
        if virtual_path != Path::new("/") {
            let parent_inode = if let Some(parent_dir) = virtual_path.parent() {
                self.get_or_create_inode(parent_dir)
            } else {
                ROOT_INODE
            };
            let dotdot_attr = self.create_file_attr(parent_inode, 0, true);
            all_entries.push(Ok(DirectoryEntryPlus {
                inode: parent_inode,
                generation: 0,
                kind: FileType::Directory,
                name: "..".into(),
                offset: (index + 1) as i64,
                attr: dotdot_attr,
                entry_ttl: self.ttl,
                attr_ttl: self.ttl,
            }));
            index += 1;
        }

        for (name, entry_inode, file_type) in entries {
            let is_dir = file_type == FileType::Directory;
            let mut attr = self.create_file_attr(entry_inode, 0, is_dir);

            // For files, try to get size from cache or real file
            if !is_dir {
                let entry_virtual_path = if virtual_path == Path::new("/") {
                    PathBuf::from(&name)
                } else {
                    virtual_path.join(&name)
                };
                if let Some(real_path) = self.get_real_path(&entry_virtual_path) {
                    let original_size = std::fs::metadata(&real_path).map(|m| m.len()).unwrap_or(0);
                    attr.size = original_size;
                    attr.blocks = original_size.div_ceil(512);
                    self.preserve_original_timestamps(&mut attr, &real_path);
                }
            }

            all_entries.push(Ok(DirectoryEntryPlus {
                inode: entry_inode,
                generation: 0,
                kind: file_type,
                name: name.into(),
                offset: (index + 1) as i64,
                attr,
                entry_ttl: self.ttl,
                attr_ttl: self.ttl,
            }));
            index += 1;
        }

        let stream = stream::iter(all_entries.into_iter().skip(offset as usize));

        Ok(ReplyDirectoryPlus {
            entries: Box::pin(stream),
        })
    }
}
