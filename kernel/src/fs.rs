// kernel/src/sfs.rs
//! Simple File System (SFS) with write caching
//!
//! Features:
//! - Block-level write caching (BufferCache)
//! - Dirty block tracking for efficient sync
//! - LRU eviction for cache management

use crate::virtio_blk::VirtioBlock;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

// Must match mkfs constants
const MAGIC: u32 = 0x53465331;
const SEC_SUPER: u64 = 0;
const SEC_MAP_START: u64 = 1;
pub const SEC_DIR_START: u64 = 65;
pub const SEC_DIR_COUNT: u64 = 64;

/// Maximum number of cached blocks
const CACHE_MAX_BLOCKS: usize = 64;

/// Cache entry access counter for LRU
static CACHE_ACCESS_COUNTER: AtomicU64 = AtomicU64::new(0);

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct DirEntry {
    name: [u8; 24],
    size: u32,
    head: u32,
}

/// Information about a file in the filesystem
/// Used by the scripting engine to expose directory listing
#[derive(Clone)]
pub struct FileInfo {
    pub name: String,
    pub size: u32,
    pub is_dir: bool,
}

// ═══════════════════════════════════════════════════════════════════════════════
// BUFFER CACHE - Block-level write caching
// ═══════════════════════════════════════════════════════════════════════════════

/// A cached block entry
struct CacheEntry {
    /// Block data
    data: [u8; 512],
    /// Whether this block has been modified
    dirty: bool,
    /// Last access time (for LRU eviction)
    last_access: u64,
}

impl CacheEntry {
    fn new(data: [u8; 512]) -> Self {
        Self {
            data,
            dirty: false,
            last_access: CACHE_ACCESS_COUNTER.fetch_add(1, Ordering::Relaxed),
        }
    }

    fn touch(&mut self) {
        self.last_access = CACHE_ACCESS_COUNTER.fetch_add(1, Ordering::Relaxed);
    }
}

/// Block cache for reducing disk I/O
pub struct BufferCache {
    /// Cached blocks: sector -> entry
    blocks: BTreeMap<u64, CacheEntry>,
    /// Number of cache hits
    hits: u64,
    /// Number of cache misses
    misses: u64,
    /// Number of writebacks
    writebacks: u64,
}

impl BufferCache {
    pub const fn new() -> Self {
        Self {
            blocks: BTreeMap::new(),
            hits: 0,
            misses: 0,
            writebacks: 0,
        }
    }

    /// Read a block, using cache if available
    #[allow(dead_code)]
    pub fn read(&mut self, dev: &mut VirtioBlock, sector: u64) -> Result<&[u8; 512], &'static str> {
        // Check cache first
        if self.blocks.contains_key(&sector) {
            self.hits += 1;
            let entry = self.blocks.get_mut(&sector).unwrap();
            entry.touch();
            return Ok(&entry.data);
        }

        // Cache miss - read from disk
        self.misses += 1;
        let mut data = [0u8; 512];
        dev.read_sector(sector, &mut data)?;

        // Evict if cache is full
        if self.blocks.len() >= CACHE_MAX_BLOCKS {
            self.evict_lru(dev)?;
        }

        // Insert into cache
        self.blocks.insert(sector, CacheEntry::new(data));
        Ok(&self.blocks.get(&sector).unwrap().data)
    }

    /// Read a block into a mutable buffer (for modification)
    pub fn read_mut(
        &mut self,
        dev: &mut VirtioBlock,
        sector: u64,
    ) -> Result<&mut [u8; 512], &'static str> {
        // Ensure block is in cache
        if !self.blocks.contains_key(&sector) {
            self.misses += 1;
            let mut data = [0u8; 512];
            dev.read_sector(sector, &mut data)?;

            if self.blocks.len() >= CACHE_MAX_BLOCKS {
                self.evict_lru(dev)?;
            }

            self.blocks.insert(sector, CacheEntry::new(data));
        } else {
            self.hits += 1;
        }

        let entry = self.blocks.get_mut(&sector).unwrap();
        entry.touch();
        Ok(&mut entry.data)
    }

    /// Write a block (cached, not immediately flushed)
    pub fn write(
        &mut self,
        dev: &mut VirtioBlock,
        sector: u64,
        data: &[u8; 512],
    ) -> Result<(), &'static str> {
        // Evict if cache is full
        if !self.blocks.contains_key(&sector) && self.blocks.len() >= CACHE_MAX_BLOCKS {
            self.evict_lru(dev)?;
        }

        // Insert or update in cache
        if let Some(entry) = self.blocks.get_mut(&sector) {
            entry.data.copy_from_slice(data);
            entry.dirty = true;
            entry.touch();
        } else {
            let mut entry = CacheEntry::new(*data);
            entry.dirty = true;
            self.blocks.insert(sector, entry);
        }

        Ok(())
    }

    /// Mark a cached block as dirty
    pub fn mark_dirty(&mut self, sector: u64) {
        if let Some(entry) = self.blocks.get_mut(&sector) {
            entry.dirty = true;
        }
    }

    /// Flush all dirty blocks to disk
    pub fn sync(&mut self, dev: &mut VirtioBlock) -> Result<usize, &'static str> {
        let mut count = 0;
        for (&sector, entry) in self.blocks.iter_mut() {
            if entry.dirty {
                dev.write_sector(sector, &entry.data)?;
                entry.dirty = false;
                self.writebacks += 1;
                count += 1;
            }
        }
        Ok(count)
    }

    /// Flush a specific block to disk
    #[allow(dead_code)]
    pub fn sync_block(&mut self, dev: &mut VirtioBlock, sector: u64) -> Result<bool, &'static str> {
        if let Some(entry) = self.blocks.get_mut(&sector) {
            if entry.dirty {
                dev.write_sector(sector, &entry.data)?;
                entry.dirty = false;
                self.writebacks += 1;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Evict the least recently used block
    fn evict_lru(&mut self, dev: &mut VirtioBlock) -> Result<(), &'static str> {
        // Find LRU entry
        let lru_sector = self
            .blocks
            .iter()
            .min_by_key(|(_, e)| e.last_access)
            .map(|(&s, _)| s);

        if let Some(sector) = lru_sector {
            // Write back if dirty
            if let Some(entry) = self.blocks.get(&sector) {
                if entry.dirty {
                    dev.write_sector(sector, &entry.data)?;
                    self.writebacks += 1;
                }
            }
            self.blocks.remove(&sector);
        }

        Ok(())
    }

    /// Invalidate a cached block (e.g., after external modification)
    #[allow(dead_code)]
    pub fn invalidate(&mut self, sector: u64) {
        self.blocks.remove(&sector);
    }

    /// Clear the entire cache (flushes dirty blocks first)
    #[allow(dead_code)]
    pub fn clear(&mut self, dev: &mut VirtioBlock) -> Result<(), &'static str> {
        self.sync(dev)?;
        self.blocks.clear();
        Ok(())
    }

    /// Get cache statistics
    pub fn stats(&self) -> (u64, u64, u64, usize) {
        (self.hits, self.misses, self.writebacks, self.blocks.len())
    }

    /// Get number of dirty blocks
    pub fn dirty_count(&self) -> usize {
        self.blocks.values().filter(|e| e.dirty).count()
    }
}

pub struct FileSystem {
    // Only cache first sector of bitmap for now to save RAM
    // A production FS would cache on demand
    bitmap_cache: [u8; 512],
    bitmap_dirty: bool,
    /// Block cache for improved performance
    cache: BufferCache,
}

impl FileSystem {
    pub fn init(dev: &mut VirtioBlock) -> Option<Self> {
        let mut buf = [0u8; 512];
        if dev.read_sector(SEC_SUPER, &mut buf).is_err() {
            return None;
        }

        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        if magic != MAGIC {
            return None;
        }

        // Load first sector of bitmap
        if dev.read_sector(SEC_MAP_START, &mut buf).is_err() {
            return None;
        }

        Some(Self {
            bitmap_cache: buf,
            bitmap_dirty: false,
            cache: BufferCache::new(),
        })
    }

    /// Sync all cached data to disk
    pub fn sync(&mut self, dev: &mut VirtioBlock) -> Result<usize, &'static str> {
        // Sync bitmap if dirty
        if self.bitmap_dirty {
            dev.write_sector(SEC_MAP_START, &self.bitmap_cache)?;
            self.bitmap_dirty = false;
        }

        // Sync block cache
        self.cache.sync(dev)
    }

    /// Get cache statistics: (hits, misses, writebacks, cached_blocks)
    pub fn cache_stats(&self) -> (u64, u64, u64, usize) {
        self.cache.stats()
    }

    /// Get number of dirty blocks waiting to be written
    pub fn dirty_blocks(&self) -> usize {
        self.cache.dirty_count() + if self.bitmap_dirty { 1 } else { 0 }
    }

    /// Get disk usage statistics: (used_blocks, total_blocks)
    /// 
    /// This counts set bits in the bitmap to determine used blocks.
    /// The bitmap tracks which 512-byte sectors are allocated.
    pub fn disk_stats(&self) -> (u64, u64) {
        // Count set bits in the bitmap (used blocks)
        let mut used_blocks: u64 = 0;
        for byte in self.bitmap_cache.iter() {
            used_blocks += byte.count_ones() as u64;
        }
        
        // Total blocks in the first bitmap sector = 512 * 8 = 4096 blocks
        // Each bit represents one 512-byte block
        let total_blocks: u64 = (self.bitmap_cache.len() * 8) as u64;
        
        (used_blocks, total_blocks)
    }

    /// Get disk usage in bytes: (used_bytes, total_bytes)
    pub fn disk_usage_bytes(&self) -> (u64, u64) {
        let (used_blocks, total_blocks) = self.disk_stats();
        (used_blocks * 512, total_blocks * 512)
    }

    /// List all files in the root directory
    /// Returns a Vec of FileInfo structs for use by the scripting engine
    pub fn list_dir(&mut self, dev: &mut VirtioBlock, _path: &str) -> Vec<FileInfo> {
        let mut entries = Vec::new();
        let mut consecutive_empty = 0;

        for i in 0..SEC_DIR_COUNT {
            let sector = SEC_DIR_START + i;
            // Use cache for faster repeated access
            let buf = match self.cache.read_mut(dev, sector) {
                Ok(b) => b,
                Err(_) => break,
            };

            let mut sector_empty = true;
            for j in 0..16 {
                // 512 / 32 = 16 entries
                let offset = j * 32;
                if buf[offset] == 0 {
                    continue;
                }

                sector_empty = false;
                let entry = unsafe { &*(buf[offset..offset + 32].as_ptr() as *const DirEntry) };

                // Decode Name
                let name_len = entry.name.iter().position(|&c| c == 0).unwrap_or(24);
                let name = core::str::from_utf8(&entry.name[..name_len])
                    .unwrap_or("???")
                    .into();

                entries.push(FileInfo {
                    name,
                    size: entry.size,
                    is_dir: false, // Simple FS - everything is a file
                });
            }

            // Early exit: if we see 2 consecutive empty sectors, stop scanning
            // (files are allocated sequentially, so gaps are unlikely)
            if sector_empty {
                consecutive_empty += 1;
                if consecutive_empty >= 2 {
                    break;
                }
            } else {
                consecutive_empty = 0;
            }
        }
        entries
    }

    /// Legacy ls function that prints directly to UART
    pub fn ls(&mut self, dev: &mut VirtioBlock) {
        crate::uart::write_line("SIZE        NAME");
        crate::uart::write_line("----------  --------------------");

        let mut consecutive_empty = 0;
        for i in 0..SEC_DIR_COUNT {
            let sector = SEC_DIR_START + i;
            let buf = match self.cache.read_mut(dev, sector) {
                Ok(b) => b,
                Err(_) => break,
            };

            let mut sector_empty = true;
            for j in 0..16 {
                // 512 / 32 = 16 entries
                let offset = j * 32;
                if buf[offset] == 0 {
                    continue;
                }

                sector_empty = false;
                let entry = unsafe { &*(buf[offset..offset + 32].as_ptr() as *const DirEntry) };

                // Decode Name
                let name_len = entry.name.iter().position(|&c| c == 0).unwrap_or(24);
                let name = core::str::from_utf8(&entry.name[..name_len]).unwrap_or("???");

                // Print
                crate::uart::write_u64(entry.size as u64);
                if entry.size < 10 {
                    crate::uart::write_str("         ");
                } else if entry.size < 100 {
                    crate::uart::write_str("        ");
                } else {
                    crate::uart::write_str("       ");
                }
                crate::uart::write_line(name);
            }

            if sector_empty {
                consecutive_empty += 1;
                if consecutive_empty >= 2 {
                    break;
                }
            } else {
                consecutive_empty = 0;
            }
        }
    }

    pub fn read_file(&self, dev: &mut VirtioBlock, filename: &str) -> Option<Vec<u8>> {
        let entry = self.find_entry(dev, filename)?;
        let mut data = Vec::with_capacity(entry.size as usize);
        let mut next = entry.head;
        let mut buf = [0u8; 512];

        while next != 0 && (data.len() < entry.size as usize) {
            dev.read_sector(next as u64, &mut buf).ok()?;
            let next_ptr = u32::from_le_bytes(buf[0..4].try_into().unwrap());

            let remaining = entry.size as usize - data.len();
            let chunk = core::cmp::min(remaining, 508);
            data.extend_from_slice(&buf[4..4 + chunk]);

            next = next_ptr;
        }
        Some(data)
    }

    pub fn write_file(
        &mut self,
        dev: &mut VirtioBlock,
        filename: &str,
        data: &[u8],
    ) -> Result<(), &'static str> {
        // Simple implementation: Overwrite existing or Create new
        let (sector, index) = match self.find_entry_pos(dev, filename) {
            Some(pos) => pos,
            None => self.find_free_dir_entry(dev).ok_or("Root dir full")?,
        };

        // Note: This implementation leaks old blocks if overwriting (simplification)

        // Write Data (using cache for better performance)
        let mut remaining = data;
        let mut head = 0;
        let mut prev = 0;

        // Special case: empty file
        if data.is_empty() {
            // head stays 0
        } else {
            while !remaining.is_empty() {
                let current = self.alloc_block(dev).ok_or("Disk full")?;
                if head == 0 {
                    head = current;
                }

                if prev != 0 {
                    // Link previous (using cache)
                    self.link_block_cached(dev, prev, current)?;
                }

                let len = core::cmp::min(remaining.len(), 508);
                let mut buf = [0u8; 512];
                // Next = 0 (for now)
                buf[4..4 + len].copy_from_slice(&remaining[..len]);

                // Write to cache instead of directly to disk
                self.cache.write(dev, current as u64, &buf)?;

                remaining = &remaining[len..];
                prev = current;
            }
        }

        // Update Dir Entry
        let mut name = [0u8; 24];
        let fname_bytes = filename.as_bytes();
        let len = core::cmp::min(fname_bytes.len(), 24);
        name[..len].copy_from_slice(&fname_bytes[..len]);

        let entry = DirEntry {
            name,
            size: data.len() as u32,
            head,
        };

        // Write Entry (using cache)
        {
            let buf = self.cache.read_mut(dev, sector)?;
            let offset = index * 32;
            let ptr = &mut buf[offset] as *mut u8 as *mut DirEntry;
            unsafe {
                *ptr = entry;
            }
        }
        self.cache.mark_dirty(sector);

        // Note: sync() is NOT called here - writes are cached until explicit sync()
        // Call fs.sync() when you need durability (e.g., after closing a file)

        Ok(())
    }

    /// Link two blocks using cached writes
    fn link_block_cached(
        &mut self,
        dev: &mut VirtioBlock,
        prev: u32,
        next: u32,
    ) -> Result<(), &'static str> {
        let buf = self.cache.read_mut(dev, prev as u64)?;
        buf[0..4].copy_from_slice(&next.to_le_bytes());
        self.cache.mark_dirty(prev as u64);
        Ok(())
    }

    // --- Helpers ---

    fn find_entry(&self, dev: &mut VirtioBlock, name: &str) -> Option<DirEntry> {
        if let Some((sec, idx)) = self.find_entry_pos(dev, name) {
            let mut buf = [0u8; 512];
            dev.read_sector(sec, &mut buf).ok()?;
            let offset = idx * 32;
            let entry = unsafe { &*(buf[offset..offset + 32].as_ptr() as *const DirEntry) };
            return Some(*entry);
        }
        None
    }

    fn find_entry_pos(&self, dev: &mut VirtioBlock, name: &str) -> Option<(u64, usize)> {
        let mut buf = [0u8; 512];
        for i in 0..SEC_DIR_COUNT {
            let sector = SEC_DIR_START + i;
            dev.read_sector(sector, &mut buf).ok()?;
            for j in 0..16 {
                let offset = j * 32;
                if buf[offset] == 0 {
                    continue;
                }
                let entry = unsafe { &*(buf[offset..offset + 32].as_ptr() as *const DirEntry) };
                let len = entry.name.iter().position(|&c| c == 0).unwrap_or(24);
                let entry_name = core::str::from_utf8(&entry.name[..len]).unwrap_or("");
                if entry_name == name {
                    return Some((sector, j));
                }
            }
        }
        None
    }

    fn find_free_dir_entry(&self, dev: &mut VirtioBlock) -> Option<(u64, usize)> {
        let mut buf = [0u8; 512];
        for i in 0..SEC_DIR_COUNT {
            let sector = SEC_DIR_START + i;
            dev.read_sector(sector, &mut buf).ok()?;
            for j in 0..16 {
                if buf[j * 32] == 0 {
                    return Some((sector, j));
                }
            }
        }
        None
    }

    fn alloc_block(&mut self, _dev: &mut VirtioBlock) -> Option<u32> {
        // Naive: Only searches the cached first sector of bitmap
        for i in 0..self.bitmap_cache.len() {
            if self.bitmap_cache[i] != 0xFF {
                for bit in 0..8 {
                    if (self.bitmap_cache[i] & (1 << bit)) == 0 {
                        self.bitmap_cache[i] |= 1 << bit;
                        self.bitmap_dirty = true;
                        // Bitmap will be synced on next fs.sync() call

                        let sector = (i * 8 + bit) as u32;
                        // Map offset + offset in map
                        // Actually our logic says sector is absolute index.
                        // But remember MKFS reserved first X sectors.
                        return Some(sector);
                    }
                }
            }
        }
        None
    }

    fn link_block(&self, dev: &mut VirtioBlock, prev: u32, next: u32) -> Result<(), &'static str> {
        let mut buf = [0u8; 512];
        dev.read_sector(prev as u64, &mut buf)?;
        buf[0..4].copy_from_slice(&next.to_le_bytes());
        dev.write_sector(prev as u64, &buf)
    }

    /// Create a directory (creates a placeholder file with trailing /)
    /// In SFS, directories are represented by files with names ending in /
    /// and containing references to their children
    pub fn mkdir(&mut self, dev: &mut VirtioBlock, path: &str) -> Result<(), &'static str> {
        // Normalize path - ensure it ends with /
        let dir_path = if path.ends_with('/') {
            String::from(path)
        } else {
            let mut s = String::from(path);
            s.push('/');
            s
        };

        // Check if directory already exists
        if self.find_entry_pos(dev, &dir_path).is_some() {
            return Err("Directory already exists");
        }

        // Create a placeholder file for the directory
        // The directory "file" contains a simple marker
        self.write_file(dev, &dir_path, b"DIR")?;

        Ok(())
    }

    /// Remove a file or empty directory
    pub fn remove(&mut self, dev: &mut VirtioBlock, path: &str) -> Result<(), &'static str> {
        let (sector, index) = self.find_entry_pos(dev, path).ok_or("File not found")?;

        // Check if it's a directory with children
        if path.ends_with('/') {
            // Check for children
            let files = self.list_dir(dev, path);
            if !files.is_empty() {
                return Err("Directory not empty");
            }
        }

        // Zero out the directory entry
        let buf = self.cache.read_mut(dev, sector)?;
        let offset = index * 32;
        for i in 0..32 {
            buf[offset + i] = 0;
        }
        self.cache.mark_dirty(sector);

        // Note: This doesn't free the data blocks (simplification)
        // A production FS would mark them as free in the bitmap

        self.cache.sync(dev)?;
        Ok(())
    }

    /// Check if a path exists
    pub fn exists(&self, dev: &mut VirtioBlock, path: &str) -> bool {
        self.find_entry_pos(dev, path).is_some()
    }

    /// Check if a path is a directory
    pub fn is_dir(&mut self, dev: &mut VirtioBlock, path: &str) -> bool {
        // Check if path ends with / or has children
        if path.ends_with('/') {
            return self.find_entry_pos(dev, path).is_some();
        }

        // Check if there are files under this path
        let dir_path = {
            let mut s = String::from(path);
            s.push('/');
            s
        };

        let files = self.list_dir(dev, "/");
        files.iter().any(|f| f.name.starts_with(&dir_path))
    }
}
