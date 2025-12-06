//! System Information MMIO Device
//!
//! Provides a communication channel between the guest kernel and the host emulator
//! for system metrics like memory usage, disk usage, etc.
//!
//! ## Register Layout (all 64-bit values are 8-byte aligned for RISC-V compatibility)
//!
//! | Offset | Name             | Access | Description                              |
//! |--------|------------------|--------|------------------------------------------|
//! | 0x00   | HEAP_USED        | R/W    | Heap used bytes (64 bits)                |
//! | 0x08   | HEAP_TOTAL       | R/W    | Heap total bytes (64 bits)               |
//! | 0x10   | DISK_USED        | R/W    | Disk used bytes (64 bits)                |
//! | 0x18   | DISK_TOTAL       | R/W    | Disk total bytes (64 bits)               |
//! | 0x20   | CPU_COUNT        | R/W    | Number of CPUs/harts (32 bits, padded)   |
//! | 0x28   | UPTIME           | R/W    | Uptime in ms (64 bits)                   |
//!
//! The kernel writes to these registers, and the emulator reads them.

use std::sync::atomic::{AtomicU64, AtomicU32, Ordering};

/// Base address for the system info device
pub const SYSINFO_BASE: u64 = 0x0011_0000;
/// Size of the system info MMIO region
pub const SYSINFO_SIZE: u64 = 0x1000;

// Register offsets (all 64-bit registers are 8-byte aligned)
const HEAP_USED: u64 = 0x00;
const HEAP_TOTAL: u64 = 0x08;
const DISK_USED: u64 = 0x10;
const DISK_TOTAL: u64 = 0x18;
const CPU_COUNT: u64 = 0x20;
// 0x24 is padding for alignment
const UPTIME: u64 = 0x28;

/// System information device for kernel-to-host communication
pub struct SysInfo {
    /// Heap memory used (in bytes)
    heap_used: AtomicU64,
    /// Heap memory total (in bytes)
    heap_total: AtomicU64,
    /// Disk space used (in bytes)
    disk_used: AtomicU64,
    /// Disk space total (in bytes)
    disk_total: AtomicU64,
    /// Number of CPUs/harts
    cpu_count: AtomicU32,
    /// System uptime in milliseconds
    uptime_ms: AtomicU64,
}

impl SysInfo {
    pub fn new() -> Self {
        Self {
            heap_used: AtomicU64::new(0),
            heap_total: AtomicU64::new(0),
            disk_used: AtomicU64::new(0),
            disk_total: AtomicU64::new(0),
            cpu_count: AtomicU32::new(1),
            uptime_ms: AtomicU64::new(0),
        }
    }

    /// Get heap memory usage (used, total) in bytes
    pub fn heap_usage(&self) -> (u64, u64) {
        (
            self.heap_used.load(Ordering::Relaxed),
            self.heap_total.load(Ordering::Relaxed),
        )
    }

    /// Get disk usage (used, total) in bytes
    pub fn disk_usage(&self) -> (u64, u64) {
        (
            self.disk_used.load(Ordering::Relaxed),
            self.disk_total.load(Ordering::Relaxed),
        )
    }

    /// Get CPU count
    pub fn cpu_count(&self) -> u32 {
        self.cpu_count.load(Ordering::Relaxed)
    }

    /// Get uptime in milliseconds
    pub fn uptime_ms(&self) -> u64 {
        self.uptime_ms.load(Ordering::Relaxed)
    }

    /// Load from register
    pub fn load(&self, offset: u64, size: u64) -> u64 {
        match (offset, size) {
            // Heap used (64-bit at offset 0x00)
            (HEAP_USED, 4) => self.heap_used.load(Ordering::Relaxed) as u32 as u64,
            (0x04, 4) => (self.heap_used.load(Ordering::Relaxed) >> 32) as u64,
            (HEAP_USED, 8) => self.heap_used.load(Ordering::Relaxed),
            
            // Heap total (64-bit at offset 0x08)
            (HEAP_TOTAL, 4) => self.heap_total.load(Ordering::Relaxed) as u32 as u64,
            (0x0C, 4) => (self.heap_total.load(Ordering::Relaxed) >> 32) as u64,
            (HEAP_TOTAL, 8) => self.heap_total.load(Ordering::Relaxed),
            
            // Disk used (64-bit at offset 0x10)
            (DISK_USED, 4) => self.disk_used.load(Ordering::Relaxed) as u32 as u64,
            (0x14, 4) => (self.disk_used.load(Ordering::Relaxed) >> 32) as u64,
            (DISK_USED, 8) => self.disk_used.load(Ordering::Relaxed),
            
            // Disk total (64-bit at offset 0x18)
            (DISK_TOTAL, 4) => self.disk_total.load(Ordering::Relaxed) as u32 as u64,
            (0x1C, 4) => (self.disk_total.load(Ordering::Relaxed) >> 32) as u64,
            (DISK_TOTAL, 8) => self.disk_total.load(Ordering::Relaxed),
            
            // CPU count (32-bit at offset 0x20)
            (CPU_COUNT, 4) | (CPU_COUNT, 8) => self.cpu_count.load(Ordering::Relaxed) as u64,
            
            // Uptime (64-bit at offset 0x28)
            (UPTIME, 4) => self.uptime_ms.load(Ordering::Relaxed) as u32 as u64,
            (0x2C, 4) => (self.uptime_ms.load(Ordering::Relaxed) >> 32) as u64,
            (UPTIME, 8) => self.uptime_ms.load(Ordering::Relaxed),
            
            _ => 0,
        }
    }

    /// Store to register
    pub fn store(&self, offset: u64, size: u64, value: u64) {
        match (offset, size) {
            // Heap used (64-bit at offset 0x00)
            (HEAP_USED, 4) => {
                let current = self.heap_used.load(Ordering::Relaxed);
                let new = (current & 0xFFFF_FFFF_0000_0000) | (value & 0xFFFF_FFFF);
                self.heap_used.store(new, Ordering::Relaxed);
            }
            (0x04, 4) => {
                let current = self.heap_used.load(Ordering::Relaxed);
                let new = (current & 0x0000_0000_FFFF_FFFF) | ((value & 0xFFFF_FFFF) << 32);
                self.heap_used.store(new, Ordering::Relaxed);
            }
            (HEAP_USED, 8) => {
                self.heap_used.store(value, Ordering::Relaxed);
            }
            
            // Heap total (64-bit at offset 0x08)
            (HEAP_TOTAL, 4) => {
                let current = self.heap_total.load(Ordering::Relaxed);
                let new = (current & 0xFFFF_FFFF_0000_0000) | (value & 0xFFFF_FFFF);
                self.heap_total.store(new, Ordering::Relaxed);
            }
            (0x0C, 4) => {
                let current = self.heap_total.load(Ordering::Relaxed);
                let new = (current & 0x0000_0000_FFFF_FFFF) | ((value & 0xFFFF_FFFF) << 32);
                self.heap_total.store(new, Ordering::Relaxed);
            }
            (HEAP_TOTAL, 8) => {
                self.heap_total.store(value, Ordering::Relaxed);
            }
            
            // Disk used (64-bit at offset 0x10)
            (DISK_USED, 4) => {
                let current = self.disk_used.load(Ordering::Relaxed);
                let new = (current & 0xFFFF_FFFF_0000_0000) | (value & 0xFFFF_FFFF);
                self.disk_used.store(new, Ordering::Relaxed);
            }
            (0x14, 4) => {
                let current = self.disk_used.load(Ordering::Relaxed);
                let new = (current & 0x0000_0000_FFFF_FFFF) | ((value & 0xFFFF_FFFF) << 32);
                self.disk_used.store(new, Ordering::Relaxed);
            }
            (DISK_USED, 8) => {
                self.disk_used.store(value, Ordering::Relaxed);
            }
            
            // Disk total (64-bit at offset 0x18)
            (DISK_TOTAL, 4) => {
                let current = self.disk_total.load(Ordering::Relaxed);
                let new = (current & 0xFFFF_FFFF_0000_0000) | (value & 0xFFFF_FFFF);
                self.disk_total.store(new, Ordering::Relaxed);
            }
            (0x1C, 4) => {
                let current = self.disk_total.load(Ordering::Relaxed);
                let new = (current & 0x0000_0000_FFFF_FFFF) | ((value & 0xFFFF_FFFF) << 32);
                self.disk_total.store(new, Ordering::Relaxed);
            }
            (DISK_TOTAL, 8) => {
                self.disk_total.store(value, Ordering::Relaxed);
            }
            
            // CPU count (32-bit at offset 0x20)
            (CPU_COUNT, 4) | (CPU_COUNT, 8) => {
                self.cpu_count.store(value as u32, Ordering::Relaxed);
            }
            
            // Uptime (64-bit at offset 0x28)
            (UPTIME, 4) => {
                let current = self.uptime_ms.load(Ordering::Relaxed);
                let new = (current & 0xFFFF_FFFF_0000_0000) | (value & 0xFFFF_FFFF);
                self.uptime_ms.store(new, Ordering::Relaxed);
            }
            (0x2C, 4) => {
                let current = self.uptime_ms.load(Ordering::Relaxed);
                let new = (current & 0x0000_0000_FFFF_FFFF) | ((value & 0xFFFF_FFFF) << 32);
                self.uptime_ms.store(new, Ordering::Relaxed);
            }
            (UPTIME, 8) => {
                self.uptime_ms.store(value, Ordering::Relaxed);
            }
            
            _ => {}
        }
    }
}

impl Default for SysInfo {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_heap_usage() {
        let sysinfo = SysInfo::new();
        
        // Write heap stats (64-bit writes at aligned offsets)
        sysinfo.store(HEAP_USED, 8, 1024 * 1024); // 1MB used
        sysinfo.store(HEAP_TOTAL, 8, 16 * 1024 * 1024); // 16MB total
        
        let (used, total) = sysinfo.heap_usage();
        assert_eq!(used, 1024 * 1024);
        assert_eq!(total, 16 * 1024 * 1024);
    }

    #[test]
    fn test_32bit_writes() {
        let sysinfo = SysInfo::new();
        
        // Write 64-bit value as two 32-bit parts
        let value: u64 = 0x1234_5678_9ABC_DEF0;
        sysinfo.store(HEAP_USED, 4, value & 0xFFFF_FFFF);
        sysinfo.store(0x04, 4, value >> 32);
        
        let (used, _) = sysinfo.heap_usage();
        assert_eq!(used, value);
    }

    #[test]
    fn test_cpu_count() {
        let sysinfo = SysInfo::new();
        
        sysinfo.store(CPU_COUNT, 4, 4);
        assert_eq!(sysinfo.cpu_count(), 4);
    }
}

