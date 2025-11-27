use crate::dram::{Dram, MemoryError};
use crate::bus::DRAM_BASE;
use crate::net::NetworkBackend;

// MMIO register *values* expected by the xv6 VirtIO driver.
const MAGIC_VALUE: u64 = 0x7472_6976;
const VERSION: u64 = 2; // Legacy VirtIO MMIO version

const VENDOR_ID: u64 = 0x554d_4551;

// Common MMIO register offsets
const MAGIC_VALUE_OFFSET: u64 = 0x000;
const VERSION_OFFSET: u64 = 0x004;
const DEVICE_ID_OFFSET: u64 = 0x008;
const VENDOR_ID_OFFSET: u64 = 0x00c;
const DEVICE_FEATURES_OFFSET: u64 = 0x010;
const DEVICE_FEATURES_SEL_OFFSET: u64 = 0x014;
const DRIVER_FEATURES_OFFSET: u64 = 0x020;
const DRIVER_FEATURES_SEL_OFFSET: u64 = 0x024;
const GUEST_PAGE_SIZE_OFFSET: u64 = 0x028;
const QUEUE_SEL_OFFSET: u64 = 0x030;
const QUEUE_NUM_MAX_OFFSET: u64 = 0x034;
const QUEUE_NUM_OFFSET: u64 = 0x038;
const QUEUE_PFN_OFFSET: u64 = 0x040;
const QUEUE_READY_OFFSET: u64 = 0x044;
const QUEUE_NOTIFY_OFFSET: u64 = 0x050;
const INTERRUPT_STATUS_OFFSET: u64 = 0x060;
const INTERRUPT_ACK_OFFSET: u64 = 0x064;
const STATUS_OFFSET: u64 = 0x070;
const QUEUE_DESC_LOW_OFFSET: u64 = 0x080;
const QUEUE_DESC_HIGH_OFFSET: u64 = 0x084;
const QUEUE_DRIVER_LOW_OFFSET: u64 = 0x090;
const QUEUE_DRIVER_HIGH_OFFSET: u64 = 0x094;
const QUEUE_DEVICE_LOW_OFFSET: u64 = 0x0a0;
const QUEUE_DEVICE_HIGH_OFFSET: u64 = 0x0a4;
const CONFIG_GENERATION_OFFSET: u64 = 0x0fc;
const CONFIG_SPACE_OFFSET: u64 = 0x100;

// Device IDs
const VIRTIO_BLK_DEVICE_ID: u32 = 2;
const VIRTIO_NET_DEVICE_ID: u32 = 1;
const VIRTIO_RNG_DEVICE_ID: u32 = 4;
#[allow(dead_code)]
const VIRTIO_CONSOLE_DEVICE_ID: u32 = 3;

// VirtIO Block Features
#[allow(dead_code)]
const VIRTIO_BLK_F_SIZE_MAX: u64 = 1;
#[allow(dead_code)]
const VIRTIO_BLK_F_SEG_MAX: u64 = 2;
#[allow(dead_code)]
const VIRTIO_BLK_F_GEOMETRY: u64 = 4;
#[allow(dead_code)]
const VIRTIO_BLK_F_RO: u64 = 5;
#[allow(dead_code)]
const VIRTIO_BLK_F_BLK_SIZE: u64 = 6;
const VIRTIO_BLK_F_FLUSH: u64 = 9;

// VirtIO Net Features
const VIRTIO_NET_F_MAC: u64 = 5;           // Device has given MAC address
const VIRTIO_NET_F_STATUS: u64 = 16;       // Configuration status field available
#[allow(dead_code)]
const VIRTIO_NET_F_MRG_RXBUF: u64 = 15;    // Driver can merge receive buffers
#[allow(dead_code)]
const VIRTIO_NET_F_CSUM: u64 = 0;          // Device handles checksum
#[allow(dead_code)]
const VIRTIO_NET_F_GUEST_CSUM: u64 = 1;    // Driver handles checksum

// VirtIO Net Status bits
const VIRTIO_NET_S_LINK_UP: u16 = 1;

const QUEUE_SIZE: u32 = 16;

const VRING_DESC_F_NEXT: u64 = 1;
const VRING_DESC_F_WRITE: u64 = 2;

/// Trait for all VirtIO devices to implement.
pub trait VirtioDevice: Send {
    fn read(&mut self, offset: u64) -> Result<u64, MemoryError>;
    fn write(&mut self, offset: u64, val: u64, dram: &mut Dram) -> Result<(), MemoryError>;
    fn is_interrupting(&self) -> bool;
    fn device_id(&self) -> u32;
    fn reg_read_size(&self, _offset: u64) -> u64 {
        // Most registers are 4 bytes.
        // Config space (>= 0x100) might be different but for now we assume 4-byte access.
        4
    }
    
    /// Poll the device for any pending work (e.g., incoming network packets).
    /// This is called periodically by the emulator's main loop.
    /// Default implementation does nothing.
    fn poll(&mut self, _dram: &mut Dram) -> Result<(), MemoryError> {
        Ok(())
    }
}

pub struct VirtioBlock {
    driver_features: u32,
    driver_features_sel: u32,
    device_features_sel: u32,
    page_size: u32,
    queue_sel: u32,
    queue_num: u32,
    queue_desc: u64,
    queue_avail: u64,
    queue_used: u64,
    queue_ready: bool,
    interrupt_status: u32,
    status: u32,
    disk: Vec<u8>,
    last_avail_idx: u16,
    pub debug: bool,
}

impl VirtioBlock {
    pub fn new(disk_image: Vec<u8>) -> Self {
        Self {
            driver_features: 0,
            driver_features_sel: 0,
            device_features_sel: 0,
            page_size: 4096,
            queue_sel: 0,
            queue_num: 0,
            queue_desc: 0,
            queue_avail: 0,
            queue_used: 0,
            queue_ready: false,
            interrupt_status: 0,
            status: 0,
            disk: disk_image,
            last_avail_idx: 0,
            debug: false,
        }
    }

    fn phys_to_offset(&self, addr: u64) -> Result<u64, MemoryError> {
        if addr < DRAM_BASE {
            return Err(MemoryError::OutOfBounds(addr));
        }
        Ok(addr - DRAM_BASE)
    }

    fn process_queue(&mut self, dram: &mut Dram) -> Result<(), MemoryError> {
        let avail_idx_addr = self.queue_avail.wrapping_add(2);
        let avail_idx = dram.load_16(self.phys_to_offset(avail_idx_addr)?)? as u16;

        let mut processed_any = false;
        while self.last_avail_idx != avail_idx {
            let qsz = if self.queue_num > 0 { self.queue_num } else { QUEUE_SIZE };
            let ring_slot = (self.last_avail_idx as u32 % qsz) as u64;
            let head_idx_addr = self.queue_avail.wrapping_add(4).wrapping_add(ring_slot * 2);
            let head_desc_idx = dram.load_16(self.phys_to_offset(head_idx_addr)?)? as u16;

            if self.debug {
                 eprintln!("[VirtioBlock] Processing queue idx={} head_desc={}", self.last_avail_idx, head_desc_idx);
            }

            let desc_idx = head_desc_idx;

            let desc_addr0 = self.queue_desc.wrapping_add((desc_idx as u64) * 16);
            let off_desc_addr0 = self.phys_to_offset(desc_addr0)?;
            let header_addr = dram.load_64(off_desc_addr0)?;
            let header_len = dram.load_32(off_desc_addr0 + 8)?;
            let header_flags = dram.load_16(off_desc_addr0 + 12)? as u64;
            let mut next_desc_idx = dram.load_16(off_desc_addr0 + 14)?;

            if header_len < 16 {
                if self.debug {
                     eprintln!("[VirtioBlock] Header too short: {}", header_len);
                }
                // Consume malformed descriptor to avoid loop
                self.last_avail_idx = self.last_avail_idx.wrapping_add(1);
                processed_any = true;
                continue;
            }

            let off_header_addr = self.phys_to_offset(header_addr)?;
            let blk_type = dram.load_32(off_header_addr)?;
            let _blk_reserved = dram.load_32(off_header_addr + 4)?;
            let blk_sector = dram.load_64(off_header_addr + 8)?;

            if self.debug {
                 eprintln!("[VirtioBlock] Request type={} sector={}", blk_type, blk_sector);
            }

            let mut data_len_done: u32 = 0;

            if (header_flags & VRING_DESC_F_NEXT) != 0 {
                let desc2_addr = self.queue_desc.wrapping_add((next_desc_idx as u64) * 16);
                let off_desc2_addr = self.phys_to_offset(desc2_addr)?;
                let data_addr = dram.load_64(off_desc2_addr)?;
                let data_len = dram.load_32(off_desc2_addr + 8)?;
                let flags2 = dram.load_16(off_desc2_addr + 12)? as u64;
                next_desc_idx = dram.load_16(off_desc2_addr + 14)?;

                if blk_type == 0 { // IN (Read)
                    let offset = blk_sector * 512;
                    if offset + (data_len as u64) <= self.disk.len() as u64 {
                        let slice = &self.disk[offset as usize..(offset as usize + data_len as usize)];
                        dram.write_bytes(self.phys_to_offset(data_addr)?, slice)?;
                        data_len_done = data_len as u32;
                    }
                } else if blk_type == 1 { // OUT (Write)
                    let offset = blk_sector * 512;
                    if offset + (data_len as u64) <= self.disk.len() as u64 {
                        for i in 0..data_len {
                            let b = dram.load_8(self.phys_to_offset(data_addr + i as u64)?)? as u8;
                            self.disk[offset as usize + i as usize] = b;
                        }
                        data_len_done = data_len as u32;
                    }
                }

                if (flags2 & VRING_DESC_F_NEXT) != 0 {
                    let desc3_addr = self.queue_desc.wrapping_add((next_desc_idx as u64) * 16);
                    let off_desc3_addr = self.phys_to_offset(desc3_addr)?;
                    let status_addr = dram.load_64(off_desc3_addr)?;
                    dram.store_8(self.phys_to_offset(status_addr)?, 0)?; // Status: OK
                }
            }

            let used_idx_addr = self.queue_used.wrapping_add(2);
            let mut used_idx = dram.load_16(self.phys_to_offset(used_idx_addr)?)? as u16;
            let elem_addr = self.queue_used.wrapping_add(4).wrapping_add((used_idx as u64 % qsz as u64) * 8);
            let off_elem_addr = self.phys_to_offset(elem_addr)?;
            dram.store_32(off_elem_addr, head_desc_idx as u64)?;
            dram.store_32(off_elem_addr + 4, data_len_done as u64)?;
            used_idx = used_idx.wrapping_add(1);
            dram.store_16(self.phys_to_offset(used_idx_addr)?, used_idx as u64)?;

            self.last_avail_idx = self.last_avail_idx.wrapping_add(1);
            processed_any = true;
        }

        if processed_any {
            self.interrupt_status |= 1;
        }

        Ok(())
    }
}

impl VirtioDevice for VirtioBlock {
    fn device_id(&self) -> u32 {
        VIRTIO_BLK_DEVICE_ID
    }

    fn is_interrupting(&self) -> bool {
        self.interrupt_status != 0
    }

    fn read(&mut self, offset: u64) -> Result<u64, MemoryError> {
        let val = match offset {
            MAGIC_VALUE_OFFSET => MAGIC_VALUE,
            VERSION_OFFSET => VERSION,
            DEVICE_ID_OFFSET => VIRTIO_BLK_DEVICE_ID as u64,
            VENDOR_ID_OFFSET => VENDOR_ID,
            DEVICE_FEATURES_OFFSET => {
                if self.device_features_sel == 0 {
                    1u64 << VIRTIO_BLK_F_FLUSH
                } else {
                    0
                }
            }
            DEVICE_FEATURES_SEL_OFFSET => self.device_features_sel as u64,
            DRIVER_FEATURES_OFFSET => self.driver_features as u64,
            DRIVER_FEATURES_SEL_OFFSET => self.driver_features_sel as u64,
            GUEST_PAGE_SIZE_OFFSET => self.page_size as u64,
            QUEUE_NUM_MAX_OFFSET => QUEUE_SIZE as u64,
            QUEUE_SEL_OFFSET => self.queue_sel as u64,
            QUEUE_NUM_OFFSET => self.queue_num as u64,
            QUEUE_READY_OFFSET => if self.queue_ready { 1 } else { 0 },
            INTERRUPT_STATUS_OFFSET => self.interrupt_status as u64,
            STATUS_OFFSET => self.status as u64,
            CONFIG_GENERATION_OFFSET => 0,
            _ if offset >= 0x100 => {
                if offset == 0x100 {
                     let cap = self.disk.len() as u64 / 512;
                     cap & 0xffffffff
                } else if offset == 0x104 {
                     let cap = self.disk.len() as u64 / 512;
                     cap >> 32
                } else {
                    0
                }
            }
            _ => 0,
        };
        Ok(val)
    }

    fn write(&mut self, offset: u64, val: u64, dram: &mut Dram) -> Result<(), MemoryError> {
        let val32 = val as u32;

        match offset {
            DEVICE_FEATURES_SEL_OFFSET => { 
                self.device_features_sel = val32; 
            }
            DRIVER_FEATURES_OFFSET => { 
                self.driver_features = val32; 
            }
            DRIVER_FEATURES_SEL_OFFSET => { 
                self.driver_features_sel = val32; 
            }
            QUEUE_SEL_OFFSET => { 
                self.queue_sel = val32; 
            }
            QUEUE_NUM_OFFSET => { 
                self.queue_num = val32; 
            }
            GUEST_PAGE_SIZE_OFFSET => { 
                self.page_size = val32; 
            }
            QUEUE_PFN_OFFSET => {
                let pfn = val32 as u64;
                if pfn != 0 {
                    let desc = pfn * (self.page_size as u64);
                    self.queue_desc = desc;
                    self.queue_avail = desc + 16 * (self.queue_num as u64);
                    // Avail ring size: flags(2) + idx(2) + ring(2*n) + used_event(2) = 6 + 2*n
                    let avail_size = 6 + 2 * (self.queue_num as u64);
                    let used = (self.queue_avail + avail_size + (self.page_size as u64) - 1) & !((self.page_size as u64) - 1);
                    self.queue_used = used;
                    self.queue_ready = true;
                    if self.debug {
                        eprintln!("[VirtIO] Queue configured: desc=0x{:x} avail=0x{:x} used=0x{:x}", self.queue_desc, self.queue_avail, self.queue_used);
                    }
                }
            }
            QUEUE_READY_OFFSET => { 
                self.queue_ready = val32 != 0; 
            }
            QUEUE_NOTIFY_OFFSET => {
                if val32 == 0 {
                    self.process_queue(dram)?;
                }
            }
            INTERRUPT_ACK_OFFSET => {
                self.interrupt_status &= !val32;
            }
            STATUS_OFFSET => { 
                if val32 == 0 {
                    // Reset
                    self.status = 0;
                    self.queue_ready = false;
                    self.interrupt_status = 0;
                    self.last_avail_idx = 0;
                } else {
                    self.status = val32; 
                }
            }
            QUEUE_DESC_LOW_OFFSET => { 
                self.queue_desc = (self.queue_desc & 0xffffffff00000000) | (val32 as u64); 
            }
            QUEUE_DESC_HIGH_OFFSET => { 
                self.queue_desc = (self.queue_desc & 0x00000000ffffffff) | ((val32 as u64) << 32); 
            }
            QUEUE_DRIVER_LOW_OFFSET => { 
                self.queue_avail = (self.queue_avail & 0xffffffff00000000) | (val32 as u64); 
            }
            QUEUE_DRIVER_HIGH_OFFSET => { 
                self.queue_avail = (self.queue_avail & 0x00000000ffffffff) | ((val32 as u64) << 32); 
            }
            QUEUE_DEVICE_LOW_OFFSET => { 
                self.queue_used = (self.queue_used & 0xffffffff00000000) | (val32 as u64); 
            }
            QUEUE_DEVICE_HIGH_OFFSET => { 
                self.queue_used = (self.queue_used & 0x00000000ffffffff) | ((val32 as u64) << 32); 
            }
            _ => {}
        }
        Ok(())
    }
}

pub struct VirtioRng {
    driver_features: u32,
    driver_features_sel: u32,
    device_features_sel: u32,
    page_size: u32,
    queue_sel: u32,
    queue_num: u32,
    queue_desc: u64,
    queue_avail: u64,
    queue_used: u64,
    queue_ready: bool,
    interrupt_status: u32,
    status: u32,
    last_avail_idx: u16,
    pub debug: bool,
}

impl VirtioRng {
    pub fn new() -> Self {
        Self {
            driver_features: 0,
            driver_features_sel: 0,
            device_features_sel: 0,
            page_size: 4096,
            queue_sel: 0,
            queue_num: 0,
            queue_desc: 0,
            queue_avail: 0,
            queue_used: 0,
            queue_ready: false,
            interrupt_status: 0,
            status: 0,
            last_avail_idx: 0,
            debug: false,
        }
    }

    fn phys_to_offset(&self, addr: u64) -> Result<u64, MemoryError> {
        if addr < DRAM_BASE {
            return Err(MemoryError::OutOfBounds(addr));
        }
        Ok(addr - DRAM_BASE)
    }

    fn process_queue(&mut self, dram: &mut Dram) -> Result<(), MemoryError> {
        let avail_idx_addr = self.queue_avail.wrapping_add(2);
        let avail_idx = dram.load_16(self.phys_to_offset(avail_idx_addr)?)? as u16;

        let mut processed_any = false;
        while self.last_avail_idx != avail_idx {
            let ring_slot = (self.last_avail_idx as u32 % QUEUE_SIZE) as u64;
            let head_idx_addr = self.queue_avail.wrapping_add(4).wrapping_add(ring_slot * 2);
            let head_desc_idx = dram.load_16(self.phys_to_offset(head_idx_addr)?)? as u16;

            let desc_addr0 = self.queue_desc.wrapping_add((head_desc_idx as u64) * 16);
            let off_desc_addr0 = self.phys_to_offset(desc_addr0)?;
            let buffer_addr = dram.load_64(off_desc_addr0)?;
            let buffer_len = dram.load_32(off_desc_addr0 + 8)?;
            let flags = dram.load_16(off_desc_addr0 + 12)? as u64;

            if (flags & VRING_DESC_F_WRITE) != 0 {
                // Fill with pseudo-random data
                for i in 0..buffer_len {
                    dram.store_8(self.phys_to_offset(buffer_addr + i as u64)?, ((i as u8).wrapping_add(42)).into())?;
                }
            }

            let used_idx_addr = self.queue_used.wrapping_add(2);
            let mut used_idx = dram.load_16(self.phys_to_offset(used_idx_addr)?)? as u16;
            let elem_addr = self.queue_used.wrapping_add(4).wrapping_add((used_idx as u64 % QUEUE_SIZE as u64) * 8);
            let off_elem_addr = self.phys_to_offset(elem_addr)?;
            dram.store_32(off_elem_addr, head_desc_idx as u64)?;
            dram.store_32(off_elem_addr + 4, buffer_len as u64)?;
            used_idx = used_idx.wrapping_add(1);
            dram.store_16(self.phys_to_offset(used_idx_addr)?, used_idx as u64)?;

            self.last_avail_idx = self.last_avail_idx.wrapping_add(1);
            processed_any = true;
        }

        if processed_any {
            self.interrupt_status |= 1;
        }

        Ok(())
    }
}

impl VirtioDevice for VirtioRng {
    fn device_id(&self) -> u32 {
        VIRTIO_RNG_DEVICE_ID
    }

    fn is_interrupting(&self) -> bool {
        self.interrupt_status != 0
    }

    fn read(&mut self, offset: u64) -> Result<u64, MemoryError> {
        let val = match offset {
            MAGIC_VALUE_OFFSET => MAGIC_VALUE,
            VERSION_OFFSET => VERSION,
            DEVICE_ID_OFFSET => VIRTIO_RNG_DEVICE_ID as u64,
            VENDOR_ID_OFFSET => VENDOR_ID,
            DEVICE_FEATURES_OFFSET => 0,
            DEVICE_FEATURES_SEL_OFFSET => self.device_features_sel as u64,
            DRIVER_FEATURES_OFFSET => self.driver_features as u64,
            DRIVER_FEATURES_SEL_OFFSET => self.driver_features_sel as u64,
            GUEST_PAGE_SIZE_OFFSET => self.page_size as u64,
            QUEUE_NUM_MAX_OFFSET => QUEUE_SIZE as u64,
            QUEUE_SEL_OFFSET => self.queue_sel as u64,
            QUEUE_NUM_OFFSET => self.queue_num as u64,
            QUEUE_READY_OFFSET => if self.queue_ready { 1 } else { 0 },
            INTERRUPT_STATUS_OFFSET => self.interrupt_status as u64,
            STATUS_OFFSET => self.status as u64,
            CONFIG_GENERATION_OFFSET => 0,
            _ => 0,
        };
        Ok(val)
    }

    fn write(&mut self, offset: u64, val: u64, dram: &mut Dram) -> Result<(), MemoryError> {
        let val32 = val as u32;
        match offset {
            DEVICE_FEATURES_SEL_OFFSET => { self.device_features_sel = val32; }
            DRIVER_FEATURES_OFFSET => { self.driver_features = val32; }
            DRIVER_FEATURES_SEL_OFFSET => { self.driver_features_sel = val32; }
            QUEUE_SEL_OFFSET => { self.queue_sel = val32; }
            QUEUE_NUM_OFFSET => { self.queue_num = val32; }
            GUEST_PAGE_SIZE_OFFSET => { self.page_size = val32; }
            QUEUE_PFN_OFFSET => {
                let pfn = val32 as u64;
                if pfn != 0 {
                    let desc = pfn * (self.page_size as u64);
                    self.queue_desc = desc;
                    self.queue_avail = desc + 16 * (self.queue_num as u64);
                    // Avail ring size: flags(2) + idx(2) + ring(2*n) + used_event(2) = 6 + 2*n
                    let avail_size = 6 + 2 * (self.queue_num as u64);
                    let used = (self.queue_avail + avail_size + (self.page_size as u64) - 1) & !((self.page_size as u64) - 1);
                    self.queue_used = used;
                    self.queue_ready = true;
                }
            }
            QUEUE_READY_OFFSET => { self.queue_ready = val32 != 0; }
            QUEUE_NOTIFY_OFFSET => {
                if val32 == 0 {
                    self.process_queue(dram)?;
                }
            }
            INTERRUPT_ACK_OFFSET => {
                self.interrupt_status &= !val32;
            }
            STATUS_OFFSET => { 
                if val32 == 0 {
                    self.status = 0;
                    self.queue_ready = false;
                    self.interrupt_status = 0;
                    self.last_avail_idx = 0;
                } else {
                    self.status = val32; 
                }
            }
            QUEUE_DESC_LOW_OFFSET => { self.queue_desc = (self.queue_desc & 0xffffffff00000000) | (val32 as u64); }
            QUEUE_DESC_HIGH_OFFSET => { self.queue_desc = (self.queue_desc & 0x00000000ffffffff) | ((val32 as u64) << 32); }
            QUEUE_DRIVER_LOW_OFFSET => { self.queue_avail = (self.queue_avail & 0xffffffff00000000) | (val32 as u64); }
            QUEUE_DRIVER_HIGH_OFFSET => { self.queue_avail = (self.queue_avail & 0x00000000ffffffff) | ((val32 as u64) << 32); }
            QUEUE_DEVICE_LOW_OFFSET => { self.queue_used = (self.queue_used & 0xffffffff00000000) | (val32 as u64); }
            QUEUE_DEVICE_HIGH_OFFSET => { self.queue_used = (self.queue_used & 0x00000000ffffffff) | ((val32 as u64) << 32); }
            _ => {}
        }
        Ok(())
    }
}

/// VirtIO Network Queue state
struct NetQueue {
    num: u32,
    desc: u64,
    avail: u64,
    used: u64,
    ready: bool,
    last_avail_idx: u16,
}

impl NetQueue {
    fn new() -> Self {
        Self {
            num: 0,
            desc: 0,
            avail: 0,
            used: 0,
            ready: false,
            last_avail_idx: 0,
        }
    }
    
    fn reset(&mut self) {
        self.num = 0;
        self.desc = 0;
        self.avail = 0;
        self.used = 0;
        self.ready = false;
        self.last_avail_idx = 0;
    }
}

/// Network statistics for monitoring and debugging (Phase 5)
#[derive(Default)]
pub struct NetStats {
    /// Packets transmitted
    pub tx_packets: u64,
    /// Packets received and delivered to guest
    pub rx_packets: u64,
    /// TX errors (send failures)
    pub tx_errors: u64,
    /// RX errors (receive/delivery failures)
    pub rx_errors: u64,
    /// Packets dropped due to no available RX buffers
    pub rx_dropped: u64,
}

/// VirtIO Network Device
/// 
/// Implements a VirtIO network device that uses a NetworkBackend
/// for actual packet I/O. Supports RX (receive) and TX (transmit) queues.
/// 
/// Config space layout (starting at offset 0x100):
/// - 0x00-0x05: MAC address (6 bytes)
/// - 0x06-0x07: Status (2 bytes) - VIRTIO_NET_S_LINK_UP if negotiated
pub struct VirtioNet {
    // Standard VirtIO fields
    driver_features: u32,
    driver_features_sel: u32,
    device_features_sel: u32,
    page_size: u32,
    queue_sel: u32,
    interrupt_status: u32,
    status: u32,
    
    // Network specific
    mac: [u8; 6],
    backend: Box<dyn NetworkBackend>,
    
    // Queues: 0 = RX, 1 = TX
    rx_queue: NetQueue,  // Queue 0: receive queue (device writes to guest)
    tx_queue: NetQueue,  // Queue 1: transmit queue (guest writes to device)
    
    // Statistics (Phase 5)
    stats: NetStats,
    
    pub debug: bool,
}

impl VirtioNet {
    /// Create a new VirtIO network device with the given backend.
    pub fn new(mut backend: Box<dyn NetworkBackend>) -> Self {
        let mac = backend.mac_address();
        
        // Initialize the backend
        if let Err(e) = backend.init() {
            log::error!("[VirtioNet] Failed to initialize backend: {}", e);
        }
        
        Self {
            driver_features: 0,
            driver_features_sel: 0,
            device_features_sel: 0,
            page_size: 4096,
            queue_sel: 0,
            interrupt_status: 0,
            status: 0,
            mac,
            backend,
            rx_queue: NetQueue::new(),
            tx_queue: NetQueue::new(),
            stats: NetStats::default(),
            debug: false,
        }
    }
    
    /// Get network statistics (Phase 5)
    pub fn get_stats(&self) -> &NetStats {
        &self.stats
    }
    
    fn phys_to_offset(&self, addr: u64) -> Result<u64, MemoryError> {
        if addr < DRAM_BASE {
            return Err(MemoryError::OutOfBounds(addr));
        }
        Ok(addr - DRAM_BASE)
    }
    
    fn current_queue(&self) -> &NetQueue {
        match self.queue_sel {
            0 => &self.rx_queue,
            1 => &self.tx_queue,
            _ => &self.rx_queue, // Default to RX for invalid selections
        }
    }
    
    fn current_queue_mut(&mut self) -> &mut NetQueue {
        match self.queue_sel {
            0 => &mut self.rx_queue,
            1 => &mut self.tx_queue,
            _ => &mut self.rx_queue,
        }
    }
    
    /// Process the RX queue - check backend for incoming packets and deliver to guest.
    /// This processes ALL available packets in a single call.
    fn process_rx_queue(&mut self, dram: &mut Dram) -> Result<(), MemoryError> {
        // Check if queue is ready
        if !self.rx_queue.ready || self.rx_queue.desc == 0 {
            return Ok(());
        }
        
        let debug = self.debug;
        let mut packets_delivered = 0;
        
        // Process all available packets from the backend
        loop {
            // Poll the backend for incoming packets
            let packet = match self.backend.recv() {
                Ok(Some(pkt)) => {
                    log::debug!("[VirtioNet] Received {} byte packet from backend", pkt.len());
                    pkt
                }
                Ok(None) => break, // No more packets available
                Err(e) => {
                    log::warn!("[VirtioNet] RX backend error: {}", e);
                    self.stats.rx_errors += 1;
                    break;
                }
            };
            
            // Extract queue state
            let queue_avail = self.rx_queue.avail;
            let queue_desc = self.rx_queue.desc;
            let queue_used = self.rx_queue.used;
            let queue_num = self.rx_queue.num;
            let last_avail_idx = self.rx_queue.last_avail_idx;
            
            let avail_idx_addr = queue_avail.wrapping_add(2);
            let avail_idx = dram.load_16(self.phys_to_offset(avail_idx_addr)?)? as u16;
            
            if last_avail_idx == avail_idx {
                // No available buffers from guest - drop the packet
                log::warn!("[VirtioNet] No RX buffers available (last_avail={}, avail={}), dropping {} byte packet", 
                    last_avail_idx, avail_idx, packet.len());
                self.stats.rx_dropped += 1;
                // Don't break - the backend has already consumed this packet, continue to next
                continue;
            }
            
            let qsz = if queue_num > 0 { queue_num } else { QUEUE_SIZE };
            let ring_slot = (last_avail_idx as u32 % qsz) as u64;
            let head_idx_addr = queue_avail.wrapping_add(4).wrapping_add(ring_slot * 2);
            let head_desc_idx = dram.load_16(self.phys_to_offset(head_idx_addr)?)? as u16;
            
            if debug {
                log::debug!("[VirtioNet] RX: Processing buffer idx={} head_desc={} pkt_len={}", 
                    last_avail_idx, head_desc_idx, packet.len());
            }
            
            // Read first descriptor - should be writable (device writes to it)
            let desc_addr = queue_desc.wrapping_add((head_desc_idx as u64) * 16);
            let off_desc = self.phys_to_offset(desc_addr)?;
            let buffer_addr = dram.load_64(off_desc)?;
            let buffer_len = dram.load_32(off_desc + 8)? as usize;
            let flags = dram.load_16(off_desc + 12)? as u64;
            
            if debug {
                log::debug!("[VirtioNet] RX desc: desc_addr=0x{:x} buffer_addr=0x{:x} len={} flags=0x{:x}", 
                    desc_addr, buffer_addr, buffer_len, flags);
            }
            
            if (flags & VRING_DESC_F_WRITE) == 0 {
                log::warn!("[VirtioNet] RX descriptor not writable");
                self.rx_queue.last_avail_idx = last_avail_idx.wrapping_add(1);
                self.stats.rx_errors += 1;
                continue;
            }
            
            // VirtIO net header (12 bytes)
            let virtio_hdr = [0u8; 12]; // All zeros - no offloading features
            let total_len = virtio_hdr.len() + packet.len();
            
            if total_len > buffer_len {
                log::warn!("[VirtioNet] Packet too large for buffer ({} > {})", total_len, buffer_len);
                self.rx_queue.last_avail_idx = last_avail_idx.wrapping_add(1);
                self.stats.rx_dropped += 1;
                continue;
            }
            
            // Write virtio header + packet data to guest buffer
            let off_buffer = self.phys_to_offset(buffer_addr)?;
            dram.write_bytes(off_buffer, &virtio_hdr)?;
            dram.write_bytes(off_buffer + virtio_hdr.len() as u64, &packet)?;
            
            // Update used ring
            let used_idx_addr = queue_used.wrapping_add(2);
            let mut used_idx = dram.load_16(self.phys_to_offset(used_idx_addr)?)? as u16;
            let elem_addr = queue_used.wrapping_add(4).wrapping_add((used_idx as u64 % qsz as u64) * 8);
            let off_elem = self.phys_to_offset(elem_addr)?;
            dram.store_32(off_elem, head_desc_idx as u64)?;
            dram.store_32(off_elem + 4, total_len as u64)?;
            used_idx = used_idx.wrapping_add(1);
            dram.store_16(self.phys_to_offset(used_idx_addr)?, used_idx as u64)?;
            
            self.rx_queue.last_avail_idx = last_avail_idx.wrapping_add(1);
            self.stats.rx_packets += 1;
            packets_delivered += 1;
            
            log::debug!("[VirtioNet] RX: Delivered {} bytes to guest", total_len);
        }
        
        // Only raise interrupt if we delivered at least one packet
        if packets_delivered > 0 {
            self.interrupt_status |= 1;
            if debug {
                log::debug!("[VirtioNet] RX: Delivered {} packets total", packets_delivered);
            }
        }
        
        Ok(())
    }
    
    /// Process the TX queue - read packets from guest and send via backend.
    fn process_tx_queue(&mut self, dram: &mut Dram) -> Result<(), MemoryError> {
        if !self.tx_queue.ready || self.tx_queue.desc == 0 {
            return Ok(());
        }
        
        // Extract queue state to avoid borrow checker issues
        let queue_avail = self.tx_queue.avail;
        let queue_desc = self.tx_queue.desc;
        let queue_used = self.tx_queue.used;
        let queue_num = self.tx_queue.num;
        let mut last_avail_idx = self.tx_queue.last_avail_idx;
        let debug = self.debug;
        
        let avail_idx_addr = queue_avail.wrapping_add(2);
        let avail_idx = dram.load_16(self.phys_to_offset(avail_idx_addr)?)? as u16;
        
        let mut processed_any = false;
        while last_avail_idx != avail_idx {
            let qsz = if queue_num > 0 { queue_num } else { QUEUE_SIZE };
            let ring_slot = (last_avail_idx as u32 % qsz) as u64;
            let head_idx_addr = queue_avail.wrapping_add(4).wrapping_add(ring_slot * 2);
            let head_desc_idx = dram.load_16(self.phys_to_offset(head_idx_addr)?)? as u16;
            
            if debug {
                log::debug!("[VirtioNet] TX: Processing buffer idx={} head_desc={}", 
                    last_avail_idx, head_desc_idx);
            }
            
            // Collect all data from descriptor chain
            let mut packet_data = Vec::new();
            let mut desc_idx = head_desc_idx;
            let mut chain_limit = 16; // Prevent infinite loops
            
            while chain_limit > 0 {
                chain_limit -= 1;
                
                let desc_addr = queue_desc.wrapping_add((desc_idx as u64) * 16);
                let off_desc = self.phys_to_offset(desc_addr)?;
                let buffer_addr = dram.load_64(off_desc)?;
                let buffer_len = dram.load_32(off_desc + 8)? as usize;
                let flags = dram.load_16(off_desc + 12)? as u64;
                let next_idx = dram.load_16(off_desc + 14)? as u16;
                
                // Read data from this descriptor
                let off_buffer = self.phys_to_offset(buffer_addr)?;
                for i in 0..buffer_len {
                    let byte = dram.load_8(off_buffer + i as u64)? as u8;
                    packet_data.push(byte);
                }
                
                if (flags & VRING_DESC_F_NEXT) == 0 {
                    break;
                }
                desc_idx = next_idx;
            }
            
            // Skip the virtio_net_hdr (12 bytes) and send the actual packet
            if packet_data.len() > 12 {
                let actual_packet = &packet_data[12..];
                if let Err(e) = self.backend.send(actual_packet) {
                    log::warn!("[VirtioNet] TX backend error: {}", e);
                    self.stats.tx_errors += 1;
                } else {
                    self.stats.tx_packets += 1;
                    if debug {
                        log::debug!("[VirtioNet] TX: Sent {} byte packet (total: {})", 
                            actual_packet.len(), self.stats.tx_packets);
                    }
                }
            }
            
            // Update used ring
            let used_idx_addr = queue_used.wrapping_add(2);
            let mut used_idx = dram.load_16(self.phys_to_offset(used_idx_addr)?)? as u16;
            let elem_addr = queue_used.wrapping_add(4).wrapping_add((used_idx as u64 % qsz as u64) * 8);
            let off_elem = self.phys_to_offset(elem_addr)?;
            dram.store_32(off_elem, head_desc_idx as u64)?;
            dram.store_32(off_elem + 4, packet_data.len() as u64)?;
            used_idx = used_idx.wrapping_add(1);
            dram.store_16(self.phys_to_offset(used_idx_addr)?, used_idx as u64)?;
            
            last_avail_idx = last_avail_idx.wrapping_add(1);
            processed_any = true;
        }
        
        // Update the actual queue state
        self.tx_queue.last_avail_idx = last_avail_idx;
        
        if processed_any {
            self.interrupt_status |= 1;
        }
        
        Ok(())
    }
    
    /// Poll for incoming packets - should be called periodically.
    /// Also processes any completed TX buffers for proper flow control.
    pub fn poll(&mut self, dram: &mut Dram) -> Result<(), MemoryError> {
        // Process any completed TX buffers first (for flow control)
        self.process_tx_queue(dram)?;
        // Then deliver any incoming RX packets
        self.process_rx_queue(dram)
    }
}

impl VirtioDevice for VirtioNet {
    fn device_id(&self) -> u32 {
        VIRTIO_NET_DEVICE_ID
    }

    fn is_interrupting(&self) -> bool {
        self.interrupt_status != 0
    }

    fn read(&mut self, offset: u64) -> Result<u64, MemoryError> {
        let val = match offset {
            MAGIC_VALUE_OFFSET => MAGIC_VALUE,
            VERSION_OFFSET => VERSION,
            DEVICE_ID_OFFSET => VIRTIO_NET_DEVICE_ID as u64,
            VENDOR_ID_OFFSET => VENDOR_ID,
            DEVICE_FEATURES_OFFSET => {
                if self.device_features_sel == 0 {
                    // Feature bits 0-31
                    (1u64 << VIRTIO_NET_F_MAC) | (1u64 << VIRTIO_NET_F_STATUS)
                } else {
                    0
                }
            }
            DEVICE_FEATURES_SEL_OFFSET => self.device_features_sel as u64,
            DRIVER_FEATURES_OFFSET => self.driver_features as u64,
            DRIVER_FEATURES_SEL_OFFSET => self.driver_features_sel as u64,
            GUEST_PAGE_SIZE_OFFSET => self.page_size as u64,
            QUEUE_NUM_MAX_OFFSET => QUEUE_SIZE as u64,
            QUEUE_SEL_OFFSET => self.queue_sel as u64,
            QUEUE_NUM_OFFSET => self.current_queue().num as u64,
            QUEUE_READY_OFFSET => if self.current_queue().ready { 1 } else { 0 },
            INTERRUPT_STATUS_OFFSET => self.interrupt_status as u64,
            STATUS_OFFSET => self.status as u64,
            CONFIG_GENERATION_OFFSET => 0,
            // Config space: MAC address at 0x100-0x105, status at 0x106-0x107
            // VirtIO MMIO accesses are 32-bit aligned, so we pack bytes into 32-bit values
            _ if offset >= CONFIG_SPACE_OFFSET => {
                let config_offset = offset - CONFIG_SPACE_OFFSET;
                // Align to 4-byte boundary and return packed value
                let aligned = config_offset & !3;
                match aligned {
                    0 => {
                        // Bytes 0-3: MAC[0..4]
                        (self.mac[0] as u64) |
                        ((self.mac[1] as u64) << 8) |
                        ((self.mac[2] as u64) << 16) |
                        ((self.mac[3] as u64) << 24)
                    }
                    4 => {
                        // Bytes 4-7: MAC[4..6], Status[0..2]
                        (self.mac[4] as u64) |
                        ((self.mac[5] as u64) << 8) |
                        ((VIRTIO_NET_S_LINK_UP as u64) << 16)
                    }
                    _ => 0,
                }
            }
            _ => 0,
        };
        Ok(val)
    }

    fn write(&mut self, offset: u64, val: u64, dram: &mut Dram) -> Result<(), MemoryError> {
        let val32 = val as u32;
        
        match offset {
            DEVICE_FEATURES_SEL_OFFSET => { 
                self.device_features_sel = val32; 
            }
            DRIVER_FEATURES_OFFSET => { 
                self.driver_features = val32; 
            }
            DRIVER_FEATURES_SEL_OFFSET => { 
                self.driver_features_sel = val32; 
            }
            QUEUE_SEL_OFFSET => { 
                self.queue_sel = val32; 
            }
            QUEUE_NUM_OFFSET => { 
                self.current_queue_mut().num = val32; 
            }
            GUEST_PAGE_SIZE_OFFSET => { 
                self.page_size = val32; 
            }
            QUEUE_PFN_OFFSET => {
                let pfn = val32 as u64;
                if pfn != 0 {
                    let page_size = self.page_size as u64;
                    let queue_sel = self.queue_sel;
                    let queue = self.current_queue_mut();
                    let desc = pfn * page_size;
                    queue.desc = desc;
                    queue.avail = desc + 16 * (queue.num as u64);
                    // Avail ring size: flags(2) + idx(2) + ring(2*n) + used_event(2) = 6 + 2*n
                    let avail_size = 6 + 2 * (queue.num as u64);
                    let used = (queue.avail + avail_size + page_size - 1) & !(page_size - 1);
                    queue.used = used;
                    queue.ready = true;
                    log::debug!("[VirtioNet] Queue {} configured: pfn={} desc=0x{:x} avail=0x{:x} used=0x{:x} num={}", 
                        queue_sel, pfn, queue.desc, queue.avail, queue.used, queue.num);
                }
            }
            QUEUE_READY_OFFSET => { 
                self.current_queue_mut().ready = val32 != 0; 
            }
            QUEUE_NOTIFY_OFFSET => {
                // val32 is the queue index being notified
                match val32 {
                    0 => {
                        // RX queue notification - guest has provided new buffers
                        // We'll try to deliver any pending packets
                        self.process_rx_queue(dram)?;
                    }
                    1 => {
                        // TX queue notification - guest has packets to send
                        self.process_tx_queue(dram)?;
                    }
                    _ => {}
                }
            }
            INTERRUPT_ACK_OFFSET => {
                self.interrupt_status &= !val32;
            }
            STATUS_OFFSET => { 
                if val32 == 0 {
                    // Reset
                    self.status = 0;
                    self.rx_queue.reset();
                    self.tx_queue.reset();
                    self.interrupt_status = 0;
                } else {
                    self.status = val32; 
                }
            }
            QUEUE_DESC_LOW_OFFSET => { 
                let queue = self.current_queue_mut();
                queue.desc = (queue.desc & 0xffffffff00000000) | (val32 as u64); 
            }
            QUEUE_DESC_HIGH_OFFSET => { 
                let queue = self.current_queue_mut();
                queue.desc = (queue.desc & 0x00000000ffffffff) | ((val32 as u64) << 32); 
            }
            QUEUE_DRIVER_LOW_OFFSET => { 
                let queue = self.current_queue_mut();
                queue.avail = (queue.avail & 0xffffffff00000000) | (val32 as u64); 
            }
            QUEUE_DRIVER_HIGH_OFFSET => { 
                let queue = self.current_queue_mut();
                queue.avail = (queue.avail & 0x00000000ffffffff) | ((val32 as u64) << 32); 
            }
            QUEUE_DEVICE_LOW_OFFSET => { 
                let queue = self.current_queue_mut();
                queue.used = (queue.used & 0xffffffff00000000) | (val32 as u64); 
            }
            QUEUE_DEVICE_HIGH_OFFSET => { 
                let queue = self.current_queue_mut();
                queue.used = (queue.used & 0x00000000ffffffff) | ((val32 as u64) << 32); 
            }
            _ => {}
        }
        Ok(())
    }
    
    fn poll(&mut self, dram: &mut Dram) -> Result<(), MemoryError> {
        self.process_rx_queue(dram)
    }
}
