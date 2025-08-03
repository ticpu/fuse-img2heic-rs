use crate::config::HeicSettings;
use aes_gcm::{
    aead::{Aead, KeyInit, OsRng},
    Aes256Gcm, Key, Nonce,
};
use anyhow::Result;
use dashmap::DashMap;
use log::{debug, info, warn};
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use std::{fs, thread, time::Duration};

/// Cache file header to track encryption status and integrity
#[derive(Debug)]
struct CacheFileHeader {
    magic: [u8; 4],     // "FHIC" magic bytes
    version: u8,        // Header version (1)
    encrypted: u8,      // 1 if encrypted, 0 if not
    quality: u8,        // HEIC quality setting when cached
    speed: u8,          // HEIC speed setting when cached
    chroma: u16,        // HEIC chroma setting when cached (big-endian)
    reserved: [u8; 16], // Reserved for future use
    checksum: [u8; 32], // SHA256 checksum of payload
    nonce: [u8; 12],    // AES-GCM nonce (only used if encrypted)
}

const CACHE_FILE_MAGIC: [u8; 4] = *b"FHIC"; // FUSE HEIC Cache
const CACHE_FILE_VERSION: u8 = 1;
const HEADER_SIZE: usize = 70; // 4+1+1+1+1+2+16+32+12

impl CacheFileHeader {
    fn new_unencrypted(payload_checksum: [u8; 32], quality: u8, speed: u8, chroma: u16) -> Self {
        Self {
            magic: CACHE_FILE_MAGIC,
            version: CACHE_FILE_VERSION,
            encrypted: 0,
            quality,
            speed,
            chroma,
            reserved: [0; 16],
            checksum: payload_checksum,
            nonce: [0; 12],
        }
    }

    fn new_encrypted(
        payload_checksum: [u8; 32],
        nonce: [u8; 12],
        quality: u8,
        speed: u8,
        chroma: u16,
    ) -> Self {
        Self {
            magic: CACHE_FILE_MAGIC,
            version: CACHE_FILE_VERSION,
            encrypted: 1,
            quality,
            speed,
            chroma,
            reserved: [0; 16],
            checksum: payload_checksum,
            nonce,
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER_SIZE);
        bytes.extend_from_slice(&self.magic);
        bytes.push(self.version);
        bytes.push(self.encrypted);
        bytes.push(self.quality);
        bytes.push(self.speed);
        bytes.extend_from_slice(&self.chroma.to_be_bytes());
        bytes.extend_from_slice(&self.reserved);
        bytes.extend_from_slice(&self.checksum);
        bytes.extend_from_slice(&self.nonce);
        bytes
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_SIZE {
            return Err(anyhow::anyhow!("Header too small"));
        }

        let magic = [bytes[0], bytes[1], bytes[2], bytes[3]];
        if magic != CACHE_FILE_MAGIC {
            return Err(anyhow::anyhow!("Invalid magic bytes"));
        }

        let version = bytes[4];
        if version != CACHE_FILE_VERSION {
            return Err(anyhow::anyhow!("Unsupported version: {}", version));
        }

        let encrypted = bytes[5];
        let quality = bytes[6];
        let speed = bytes[7];
        let chroma = u16::from_be_bytes([bytes[8], bytes[9]]);
        let mut reserved = [0u8; 16];
        reserved.copy_from_slice(&bytes[10..26]);
        let mut checksum = [0u8; 32];
        checksum.copy_from_slice(&bytes[26..58]);
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&bytes[58..70]);

        Ok(Self {
            magic,
            version,
            encrypted,
            quality,
            speed,
            chroma,
            reserved,
            checksum,
            nonce,
        })
    }

    fn is_encrypted(&self) -> bool {
        self.encrypted == 1
    }

    fn matches_heic_settings(&self, quality: u8, speed: u8, chroma: u16) -> bool {
        self.quality == quality && self.speed == speed && self.chroma == chroma
    }
}

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
    encryption_enabled: bool,
}

#[derive(Debug)]
pub struct CacheContext {
    pub filepath: String,
    pub heic_settings: HeicSettings,
}

impl CacheContext {
    pub fn new(filepath: String, heic_settings: HeicSettings) -> Self {
        Self {
            filepath,
            heic_settings,
        }
    }
}

impl ImageCache {
    pub fn new(
        max_size_mb: u64,
        cache_dir: PathBuf,
        encryption_enabled: bool,
    ) -> Result<Arc<Self>> {
        info!("Initializing cache with max size: {max_size_mb} MB, cache dir: {cache_dir:?}, encryption: {encryption_enabled}");

        fs::create_dir_all(&cache_dir)?;

        let cache = Arc::new(Self {
            data: DashMap::new(),
            access_times: DashMap::new(),
            current_size: AtomicU64::new(0),
            max_size: max_size_mb * 1024 * 1024, // Convert MB to bytes
            cache_dir,
            disk_cache_enabled: true,
            encryption_enabled,
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

    /// Generate encryption key from filepath using SHA256
    fn generate_encryption_key(&self, filepath: &str) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(filepath.as_bytes());
        hasher.update(b"fuse-img2heic-encryption-key");
        let hash = hasher.finalize();
        hash.into()
    }

    /// Encrypt data using AES-GCM with filepath-derived key
    fn encrypt_data(&self, data: &[u8], filepath: &str) -> Result<(Vec<u8>, [u8; 12])> {
        let key_bytes = self.generate_encryption_key(filepath);
        let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
        let cipher = Aes256Gcm::new(key);

        let mut nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = cipher
            .encrypt(nonce, data)
            .map_err(|e| anyhow::anyhow!("Failed to encrypt cache data: {:?}", e))?;

        Ok((ciphertext, nonce_bytes))
    }

    /// Decrypt data using AES-GCM with filepath-derived key
    fn decrypt_data(
        &self,
        encrypted_data: &[u8],
        nonce: &[u8; 12],
        filepath: &str,
    ) -> Result<Vec<u8>> {
        let key_bytes = self.generate_encryption_key(filepath);
        let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
        let cipher = Aes256Gcm::new(key);

        let nonce = Nonce::from_slice(nonce);

        let plaintext = cipher
            .decrypt(nonce, encrypted_data)
            .map_err(|e| anyhow::anyhow!("Failed to decrypt cache data: {:?}", e))?;

        Ok(plaintext)
    }

    pub fn get_with_context(&self, key: &str, context: &CacheContext) -> Option<Vec<u8>> {
        self.get(key, &context.filepath, &context.heic_settings)
    }

    pub fn get(&self, key: &str, filepath: &str, heic_settings: &HeicSettings) -> Option<Vec<u8>> {
        // Update access time first
        self.access_times.insert(key.to_string(), Instant::now());

        // Try memory cache first
        if let Some(entry) = self.data.get(key) {
            log::trace!("Cache hit (memory): {key}");
            return Some(entry.data.clone());
        }

        // Try disk cache
        if self.disk_cache_enabled {
            if let Ok(data) = self.load_from_disk_key(key, filepath, heic_settings) {
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
            } else {
                // Cache file is corrupted, encrypted with wrong key, or has mismatched settings
                debug!("Cache file corrupted or invalid for {key}, will regenerate");
                let _ = self.remove_from_disk_key(key);
            }
        }

        log::trace!("Cache miss: {key}");
        None
    }

    pub fn put_with_context(
        &self,
        key: String,
        data: Vec<u8>,
        context: &CacheContext,
    ) -> Result<()> {
        self.put(key, data, &context.filepath, &context.heic_settings)
    }

    pub fn put(
        &self,
        key: String,
        data: Vec<u8>,
        filepath: &str,
        heic_settings: &HeicSettings,
    ) -> Result<()> {
        let size = data.len() as u64;

        log::trace!("Caching entry: {key} ({size} bytes)");

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
            if let Err(e) = self.save_to_disk_key(&key, &data, filepath, heic_settings) {
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

        debug!("Validating cache files on disk");

        // Scan all subdirectories (xx format)
        for subdir_entry in fs::read_dir(&self.cache_dir)? {
            let subdir_entry = subdir_entry?;
            let subdir_path = subdir_entry.path();

            if !subdir_path.is_dir() {
                continue;
            }

            let subdir_name = match subdir_path.file_name().and_then(|n| n.to_str()) {
                Some(name) if name.len() == 2 => name,
                _ => continue, // Skip non-hash subdirectories
            };

            // Scan files in subdirectory
            for file_entry in fs::read_dir(&subdir_path)? {
                let file_entry = file_entry?;
                let file_path = file_entry.path();

                if !file_path.is_file() {
                    continue;
                }

                if let Some(filename) = file_path.file_name().and_then(|n| n.to_str()) {
                    // Reconstruct the full hash key
                    let cache_key = format!("{subdir_name}{filename}");

                    match fs::read(&file_path) {
                        Ok(file_content) => {
                            // Check if file has valid header
                            if file_content.len() < HEADER_SIZE {
                                debug!("Removing old cache file without header: {file_path:?}");
                                let _ = fs::remove_file(&file_path);
                                continue;
                            }

                            // Try to parse header - remove file if invalid
                            if CacheFileHeader::from_bytes(&file_content[..HEADER_SIZE]).is_err() {
                                debug!("Removing cache file with invalid header: {file_path:?}");
                                let _ = fs::remove_file(&file_path);
                                continue;
                            }

                            // File has valid header format but we can't load it to memory
                            // without knowing the original filepath and HEIC settings.
                            // We'll just count it towards disk usage but not load to memory.
                            debug!("Found valid cache file on disk: {cache_key}");
                        }
                        Err(e) => {
                            warn!("Failed to read cache file {file_path:?}: {e}");
                            let _ = fs::remove_file(&file_path);
                        }
                    }
                }
            }
        }

        self.current_size.store(0, Ordering::Relaxed);
        info!("Cache initialized, validated existing cache files (will be loaded on demand)");

        Ok(())
    }

    fn save_to_disk_key(
        &self,
        key: &str,
        data: &[u8],
        filepath: &str,
        heic_settings: &HeicSettings,
    ) -> Result<()> {
        let file_path = get_cache_file_path(&self.cache_dir, key);

        // Create subdirectory if it doesn't exist
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Calculate payload checksum
        let mut hasher = Sha256::new();
        hasher.update(data);
        let payload_checksum: [u8; 32] = hasher.finalize().into();

        let (final_data, header) = if self.encryption_enabled {
            // Encrypt the data
            let (encrypted_data, nonce) = self.encrypt_data(data, filepath)?;
            let header = CacheFileHeader::new_encrypted(
                payload_checksum,
                nonce,
                heic_settings.quality,
                heic_settings.speed,
                heic_settings.chroma,
            );
            (encrypted_data, header)
        } else {
            let header = CacheFileHeader::new_unencrypted(
                payload_checksum,
                heic_settings.quality,
                heic_settings.speed,
                heic_settings.chroma,
            );
            (data.to_vec(), header)
        };

        // Write header + data to file
        let mut file_content = header.to_bytes();
        file_content.extend_from_slice(&final_data);

        fs::write(file_path, file_content)?;
        Ok(())
    }

    fn load_from_disk_key(
        &self,
        key: &str,
        filepath: &str,
        heic_settings: &HeicSettings,
    ) -> Result<Vec<u8>> {
        let file_path = get_cache_file_path(&self.cache_dir, key);
        let file_content = fs::read(file_path)?;

        if file_content.len() < HEADER_SIZE {
            return Err(anyhow::anyhow!("Cache file too small"));
        }

        // Parse header
        let header = CacheFileHeader::from_bytes(&file_content[..HEADER_SIZE])?;

        // Validate HEIC settings match
        if !header.matches_heic_settings(
            heic_settings.quality,
            heic_settings.speed,
            heic_settings.chroma,
        ) {
            return Err(anyhow::anyhow!(
                "HEIC settings mismatch, cache entry invalid"
            ));
        }

        let payload = &file_content[HEADER_SIZE..];

        let decrypted_data = if header.is_encrypted() {
            if !self.encryption_enabled {
                return Err(anyhow::anyhow!(
                    "Cache file is encrypted but encryption is disabled"
                ));
            }
            self.decrypt_data(payload, &header.nonce, filepath)?
        } else {
            payload.to_vec()
        };

        // Verify checksum
        let mut hasher = Sha256::new();
        hasher.update(&decrypted_data);
        let computed_checksum: [u8; 32] = hasher.finalize().into();

        if computed_checksum != header.checksum {
            return Err(anyhow::anyhow!("Cache file checksum mismatch"));
        }

        Ok(decrypted_data)
    }

    fn remove_from_disk_key(&self, key: &str) -> Result<()> {
        let file_path = get_cache_file_path(&self.cache_dir, key);
        fs::remove_file(file_path)?;
        Ok(())
    }
}

/// Create a cache key from filepath, original file size, and HEIC settings using SHA256
/// Returns the hash that will be used for both memory cache key and disk file path
pub fn create_cache_key(
    filepath: &str,
    original_size: u64,
    heic_settings: &HeicSettings,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(filepath.as_bytes());
    hasher.update(original_size.to_le_bytes());
    hasher.update([heic_settings.quality]);
    hasher.update([heic_settings.speed]);
    hasher.update(heic_settings.chroma.to_le_bytes());

    // Include max_resolution in cache key if set
    if let Some(ref res_str) = heic_settings.max_resolution {
        hasher.update(res_str.as_bytes());
    }

    let hash = hasher.finalize();
    hex::encode(hash)
}

/// Create both cache key and context from a path and parameters
pub fn create_cache_key_and_context_for_path(
    filepath: &Path,
    original_size: u64,
    heic_settings: &HeicSettings,
) -> (String, CacheContext) {
    let filepath_str = filepath.to_string_lossy().to_string();
    let key = create_cache_key(&filepath_str, original_size, heic_settings);
    let context = CacheContext::new(filepath_str, heic_settings.clone());
    (key, context)
}

/// Get the disk file path for a cache key using the xx/xxxxx directory structure
fn get_cache_file_path(cache_dir: &Path, cache_key: &str) -> PathBuf {
    // Take first 2 characters for subdirectory, remainder for filename
    let subdir = &cache_key[0..2];
    let filename = &cache_key[2..];

    cache_dir.join(subdir).join(filename)
}
