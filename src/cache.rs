use crate::config::HeicSettings;
use aes_gcm::{
    aead::{Aead, KeyInit, OsRng},
    Aes256Gcm, Key, Nonce,
};
use anyhow::Result;
use log::{debug, info};
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
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

pub struct ImageCache {
    max_size: u64,
    cache_dir: PathBuf,
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
        info!("Initializing disk cache: max size {max_size_mb} MB, dir: {cache_dir:?}, encryption: {encryption_enabled}");

        fs::create_dir_all(&cache_dir)?;

        let cache = Arc::new(Self {
            max_size: max_size_mb * 1024 * 1024,
            cache_dir,
            encryption_enabled,
        });

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
        // Read from disk cache (Linux page cache handles hot data)
        match self.load_from_disk_key(key, filepath, heic_settings) {
            Ok(data) => {
                log::trace!("Cache hit: {key}");
                Some(data)
            }
            Err(_) => {
                log::trace!("Cache miss: {key}");
                None
            }
        }
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
        log::trace!("Caching entry: {key} ({} bytes)", data.len());
        self.save_to_disk_key(&key, &data, filepath, heic_settings)
    }

    fn cleanup_worker(&self) {
        loop {
            thread::sleep(Duration::from_secs(300)); // Run every 5 minutes
            self.enforce_disk_limit();
        }
    }

    fn enforce_disk_limit(&self) {
        // Get all cache files with their size and atime
        let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
        let mut total_size: u64 = 0;

        if let Ok(subdirs) = fs::read_dir(&self.cache_dir) {
            for subdir in subdirs.flatten() {
                if !subdir.path().is_dir() {
                    continue;
                }
                if let Ok(entries) = fs::read_dir(subdir.path()) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if let Ok(meta) = path.metadata() {
                            if meta.is_file() {
                                let size = meta.len();
                                let atime = meta.accessed().unwrap_or(std::time::UNIX_EPOCH);
                                files.push((path, size, atime));
                                total_size += size;
                            }
                        }
                    }
                }
            }
        }

        if total_size <= self.max_size {
            return;
        }

        debug!("Cache cleanup: {} bytes used, {} max", total_size, self.max_size);

        // Sort by atime (oldest first)
        files.sort_by_key(|(_, _, atime)| *atime);

        // Remove oldest files until under limit
        for (path, size, _) in files {
            if total_size <= self.max_size {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total_size -= size;
                debug!("Evicted: {path:?}");
            }
        }
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

        // AES-GCM provides authenticated encryption (integrity check on decrypt)
        // For unencrypted, we trust the filesystem
        if header.is_encrypted() {
            if !self.encryption_enabled {
                return Err(anyhow::anyhow!(
                    "Cache file is encrypted but encryption is disabled"
                ));
            }
            self.decrypt_data(payload, &header.nonce, filepath)
        } else {
            Ok(payload.to_vec())
        }
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
