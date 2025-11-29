// kernel/src/sfs.rs
use alloc::string::String;
use alloc::vec::Vec;
use crate::virtio_blk::VirtioBlock;

// Must match mkfs constants
const SECTOR_SIZE: u64 = 512;
const MAGIC: u32 = 0x53465331;
const SEC_SUPER: u64 = 0;
const SEC_MAP_START: u64 = 1;
const SEC_MAP_COUNT: u64 = 64;
pub const SEC_DIR_START: u64 = 65;
pub const SEC_DIR_COUNT: u64 = 64;
const SEC_DATA_START: u64 = 129;

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

pub struct FileSystem {
    // Only cache first sector of bitmap for now to save RAM
    // A production FS would cache on demand
    bitmap_cache: [u8; 512],
    bitmap_dirty: bool,
}

impl FileSystem {
    pub fn init(dev: &mut VirtioBlock) -> Option<Self> {
        let mut buf = [0u8; 512];
        if dev.read_sector(SEC_SUPER, &mut buf).is_err() { return None; }
        
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        if magic != MAGIC { return None; }

        // Load first sector of bitmap
        if dev.read_sector(SEC_MAP_START, &mut buf).is_err() { return None; }

        Some(Self {
            bitmap_cache: buf,
            bitmap_dirty: false,
        })
    }

    /// List all files in the root directory
    /// Returns a Vec of FileInfo structs for use by the scripting engine
    pub fn list_dir(&self, dev: &mut VirtioBlock, _path: &str) -> Vec<FileInfo> {
        let mut entries = Vec::new();
        let mut buf = [0u8; 512];

        for i in 0..SEC_DIR_COUNT {
            if dev.read_sector(SEC_DIR_START + i, &mut buf).is_ok() {
                for j in 0..16 { // 512 / 32 = 16 entries
                    let offset = j * 32;
                    if buf[offset] == 0 { continue; }

                    let entry = unsafe { &*(buf[offset..offset+32].as_ptr() as *const DirEntry) };
                    
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
            }
        }
        entries
    }

    /// Legacy ls function that prints directly to UART
    pub fn ls(&self, dev: &mut VirtioBlock) {
        let mut buf = [0u8; 512];
        crate::uart::write_line("SIZE        NAME");
        crate::uart::write_line("----------  --------------------");

        for i in 0..SEC_DIR_COUNT {
            dev.read_sector(SEC_DIR_START + i, &mut buf).ok();
            for j in 0..16 { // 512 / 32 = 16 entries
                let offset = j * 32;
                if buf[offset] == 0 { continue; }

                let entry = unsafe { &*(buf[offset..offset+32].as_ptr() as *const DirEntry) };
                
                // Decode Name
                let name_len = entry.name.iter().position(|&c| c == 0).unwrap_or(24);
                let name = core::str::from_utf8(&entry.name[..name_len]).unwrap_or("???");

                // Print
                crate::uart::write_u64(entry.size as u64);
                if entry.size < 10 { crate::uart::write_str("         "); }
                else if entry.size < 100 { crate::uart::write_str("        "); }
                else { crate::uart::write_str("       "); }
                crate::uart::write_line(name);
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
            data.extend_from_slice(&buf[4..4+chunk]);
            
            next = next_ptr;
        }
        Some(data)
    }

    pub fn write_file(&mut self, dev: &mut VirtioBlock, filename: &str, data: &[u8]) -> Result<(), &'static str> {
        // Simple implementation: Overwrite existing or Create new
        let (sector, index) = match self.find_entry_pos(dev, filename) {
            Some(pos) => pos,
            None => self.find_free_dir_entry(dev).ok_or("Root dir full")?,
        };

        // Note: This implementation leaks old blocks if overwriting (simplification)
        
        // Write Data
        let mut remaining = data;
        let mut head = 0;
        let mut prev = 0;

        // Special case: empty file
        if data.is_empty() {
            // head stays 0
        } else {
            while !remaining.is_empty() {
                let current = self.alloc_block(dev).ok_or("Disk full")?;
                if head == 0 { head = current; }
                
                if prev != 0 {
                    // Link previous
                    self.link_block(dev, prev, current)?;
                }

                let len = core::cmp::min(remaining.len(), 508);
                let mut buf = [0u8; 512];
                // Next = 0 (for now)
                buf[4..4+len].copy_from_slice(&remaining[..len]);
                dev.write_sector(current as u64, &buf)?;

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

        // Write Entry
        let mut buf = [0u8; 512];
        dev.read_sector(sector, &mut buf)?;
        let offset = index * 32;
        let ptr = &mut buf[offset] as *mut u8 as *mut DirEntry;
        unsafe { *ptr = entry; }
        dev.write_sector(sector, &buf)?;

        Ok(())
    }

    // --- Helpers ---

    fn find_entry(&self, dev: &mut VirtioBlock, name: &str) -> Option<DirEntry> {
        if let Some((sec, idx)) = self.find_entry_pos(dev, name) {
            let mut buf = [0u8; 512];
            dev.read_sector(sec, &mut buf).ok()?;
            let offset = idx * 32;
            let entry = unsafe { &*(buf[offset..offset+32].as_ptr() as *const DirEntry) };
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
                if buf[offset] == 0 { continue; }
                let entry = unsafe { &*(buf[offset..offset+32].as_ptr() as *const DirEntry) };
                let len = entry.name.iter().position(|&c| c == 0).unwrap_or(24);
                let entry_name = core::str::from_utf8(&entry.name[..len]).unwrap_or("");
                if entry_name == name { return Some((sector, j)); }
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
                if buf[j * 32] == 0 { return Some((sector, j)); }
            }
        }
        None
    }

    fn alloc_block(&mut self, dev: &mut VirtioBlock) -> Option<u32> {
        // Naive: Only searches the cached first sector of bitmap
        for i in 0..self.bitmap_cache.len() {
            if self.bitmap_cache[i] != 0xFF {
                for bit in 0..8 {
                    if (self.bitmap_cache[i] & (1 << bit)) == 0 {
                        self.bitmap_cache[i] |= 1 << bit;
                        self.bitmap_dirty = true;
                        
                        // Sync immediately
                        dev.write_sector(SEC_MAP_START, &self.bitmap_cache).ok()?;
                        
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
}