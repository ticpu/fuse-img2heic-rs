use anyhow::Result;
use dashmap::DashMap;
use log::{debug, info, warn};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use std::{fs, thread, time::Duration};

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub data: Vec<u8>,
    pub size: u64,
}

pub struct ImageCache {
    data: DashMap<String, CacheEntry>,
    access_times: DashMap<String, Instant>,
    current_size: AtomicU64,
    max_size: u64,
    cache_dir: PathBuf,
    disk_cache_enabled: bool,
}

impl ImageCache {
    pub fn new(max_size_mb: u64, cache_dir: PathBuf) -> Result<Arc<Self>> {
        info!("Initializing cache with max size: {max_size_mb} MB, cache dir: {cache_dir:?}");

        fs::create_dir_all(&cache_dir)?;

        let cache = Arc::new(Self {
            data: DashMap::new(),
            access_times: DashMap::new(),
            current_size: AtomicU64::new(0),
            max_size: max_size_mb * 1024 * 1024, // Convert MB to bytes
            cache_dir,
            disk_cache_enabled: true,
        });

        // Load existing cache entries from disk
        cache.load_from_disk()?;

        // Start background cleanup thread
        let cache_clone = Arc::clone(&cache);
        thread::spawn(move || {
            cache_clone.cleanup_worker();
        });

        Ok(cache)
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        // Update access time first
        self.access_times.insert(key.to_string(), Instant::now());

        // Try memory cache first
        if let Some(entry) = self.data.get(key) {
            debug!("Cache hit (memory): {key}");
            return Some(entry.data.clone());
        }

        // Try disk cache
        if self.disk_cache_enabled {
            if let Ok(data) = self.load_from_disk_key(key) {
                debug!("Cache hit (disk): {key}");

                // Load into memory cache if there's space
                let size = data.len() as u64;
                if self.current_size.load(Ordering::Relaxed) + size <= self.max_size {
                    let entry = CacheEntry {
                        data: data.clone(),
                        size,
                    };

                    self.data.insert(key.to_string(), entry);
                    self.current_size.fetch_add(size, Ordering::Relaxed);
                }

                return Some(data);
            }
        }

        debug!("Cache miss: {key}");
        None
    }

    pub fn put(&self, key: String, data: Vec<u8>) -> Result<()> {
        let size = data.len() as u64;

        debug!("Caching entry: {key} ({size} bytes)");

        // Check if we need to evict entries to make space
        self.ensure_space(size);

        let entry = CacheEntry {
            data: data.clone(),
            size,
        };

        // Store in memory
        self.data.insert(key.clone(), entry);
        self.access_times.insert(key.clone(), Instant::now());
        self.current_size.fetch_add(size, Ordering::Relaxed);

        // Store on disk
        if self.disk_cache_enabled {
            if let Err(e) = self.save_to_disk_key(&key, &data) {
                warn!("Failed to save cache entry to disk: {e}");
            }
        }

        Ok(())
    }

    fn ensure_space(&self, needed_size: u64) {
        let current = self.current_size.load(Ordering::Relaxed);

        if current + needed_size <= self.max_size {
            return;
        }

        debug!(
            "Cache full, evicting entries (current: {} bytes, needed: {} bytes, max: {} bytes)",
            current, needed_size, self.max_size
        );

        // Collect entries with access times for sorting
        let mut entries: Vec<(String, Instant)> = self
            .access_times
            .iter()
            .map(|item| (item.key().clone(), *item.value()))
            .collect();

        // Sort by access time (oldest first)
        entries.sort_by_key(|(_, time)| *time);

        let target_size = self.max_size.saturating_sub(needed_size);

        for (key, _) in entries {
            if self.current_size.load(Ordering::Relaxed) <= target_size {
                break;
            }

            if let Some((_, entry)) = self.data.remove(&key) {
                self.access_times.remove(&key);
                self.current_size.fetch_sub(entry.size, Ordering::Relaxed);

                debug!("Evicted cache entry: {} ({} bytes)", key, entry.size);

                // Remove from disk
                if self.disk_cache_enabled {
                    let _ = self.remove_from_disk_key(&key);
                }
            }
        }
    }

    fn cleanup_worker(&self) {
        loop {
            thread::sleep(Duration::from_secs(300)); // Run every 5 minutes

            let current_size = self.current_size.load(Ordering::Relaxed);
            let memory_entries = self.data.len();

            debug!(
                "Cache cleanup: {} entries, {} bytes used, {} bytes max",
                memory_entries, current_size, self.max_size
            );

            // If we're over 90% capacity, proactively evict some entries
            if current_size > (self.max_size * 9) / 10 {
                self.ensure_space(self.max_size / 10); // Free up 10% of cache
            }
        }
    }

    fn load_from_disk(&self) -> Result<()> {
        if !self.disk_cache_enabled {
            return Ok(());
        }

        debug!("Loading cache entries from disk");

        let entries = fs::read_dir(&self.cache_dir)?;
        let mut loaded_count = 0;
        let mut total_size = 0u64;

        for entry in entries {
            let entry = entry?;
            let path = entry.path();

            if path.is_file() && path.extension().is_some_and(|ext| ext == "cache") {
                if let Some(key) = path.file_stem().and_then(|s| s.to_str()) {
                    match fs::read(&path) {
                        Ok(data) => {
                            let size = data.len() as u64;
                            if total_size + size <= self.max_size {
                                let cache_entry = CacheEntry { data, size };

                                self.data.insert(key.to_string(), cache_entry);
                                self.access_times.insert(key.to_string(), Instant::now());
                                total_size += size;
                                loaded_count += 1;
                            }
                        }
                        Err(e) => {
                            warn!("Failed to load cache entry from {path:?}: {e}");
                            let _ = fs::remove_file(&path); // Remove corrupted file
                        }
                    }
                }
            }
        }

        self.current_size.store(total_size, Ordering::Relaxed);
        info!("Loaded {loaded_count} cache entries from disk ({total_size} bytes)");

        Ok(())
    }

    fn save_to_disk_key(&self, key: &str, data: &[u8]) -> Result<()> {
        let file_path = self
            .cache_dir
            .join(format!("{}.cache", key.replace('/', "_")));
        fs::write(file_path, data)?;
        Ok(())
    }

    fn load_from_disk_key(&self, key: &str) -> Result<Vec<u8>> {
        let file_path = self
            .cache_dir
            .join(format!("{}.cache", key.replace('/', "_")));
        Ok(fs::read(file_path)?)
    }

    fn remove_from_disk_key(&self, key: &str) -> Result<()> {
        let file_path = self
            .cache_dir
            .join(format!("{}.cache", key.replace('/', "_")));
        fs::remove_file(file_path)?;
        Ok(())
    }
}

pub fn create_cache_key(path: &str, target_size: Option<u64>) -> String {
    match target_size {
        Some(size) => format!("{path}#{size}"),
        None => path.to_string(),
    }
}
