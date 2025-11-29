//! VirtIO Network Driver for the kernel.
//!
//! This driver communicates with the VirtIO MMIO network device
//! to send and receive Ethernet frames.

use core::ptr::{read_volatile, write_volatile};

/// VirtIO MMIO base address for first device slot.
pub const VIRTIO_BASE: usize = 0x1000_1000;
/// VirtIO MMIO stride between devices.
pub const VIRTIO_STRIDE: usize = 0x1000;
/// Maximum number of VirtIO devices to scan.
pub const VIRTIO_MAX_DEVICES: usize = 8;

/// Legacy constant for compatibility.
pub const VIRTIO_NET_BASE: usize = VIRTIO_BASE;

// MMIO register offsets
const MAGIC_VALUE_OFFSET: usize = 0x000;
const VERSION_OFFSET: usize = 0x004;
const DEVICE_ID_OFFSET: usize = 0x008;
#[allow(dead_code)]
const VENDOR_ID_OFFSET: usize = 0x00c;
const DEVICE_FEATURES_OFFSET: usize = 0x010;
const DEVICE_FEATURES_SEL_OFFSET: usize = 0x014;
const DRIVER_FEATURES_OFFSET: usize = 0x020;
const DRIVER_FEATURES_SEL_OFFSET: usize = 0x024;
const GUEST_PAGE_SIZE_OFFSET: usize = 0x028;
const QUEUE_SEL_OFFSET: usize = 0x030;
const QUEUE_NUM_MAX_OFFSET: usize = 0x034;
const QUEUE_NUM_OFFSET: usize = 0x038;
const QUEUE_PFN_OFFSET: usize = 0x040;
#[allow(dead_code)]
const QUEUE_READY_OFFSET: usize = 0x044;
const QUEUE_NOTIFY_OFFSET: usize = 0x050;
const INTERRUPT_STATUS_OFFSET: usize = 0x060;
const INTERRUPT_ACK_OFFSET: usize = 0x064;
const STATUS_OFFSET: usize = 0x070;
const CONFIG_SPACE_OFFSET: usize = 0x100;

// Expected values
const VIRTIO_MAGIC: u32 = 0x7472_6976;
const VIRTIO_VERSION: u32 = 2;
const VIRTIO_NET_DEVICE_ID: u32 = 1;

// Device status bits
const STATUS_ACKNOWLEDGE: u32 = 1;
const STATUS_DRIVER: u32 = 2;
const STATUS_FEATURES_OK: u32 = 8;
const STATUS_DRIVER_OK: u32 = 4;

// Feature bits
const VIRTIO_NET_F_MAC: u32 = 1 << 5;
const VIRTIO_NET_F_STATUS: u32 = 1 << 16;

// Descriptor flags
#[allow(dead_code)]
const VRING_DESC_F_NEXT: u16 = 1;
const VRING_DESC_F_WRITE: u16 = 2;

/// Queue size (number of descriptors)
pub const QUEUE_SIZE: usize = 16;

/// Page size for queue alignment
const PAGE_SIZE: usize = 4096;

/// VirtIO descriptor structure (16 bytes)
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VirtqDesc {
    pub addr: u64,   // Physical address of buffer
    pub len: u32,    // Length of buffer
    pub flags: u16,  // Descriptor flags
    pub next: u16,   // Next descriptor index (if NEXT flag set)
}

/// VirtIO available ring
#[repr(C)]
pub struct VirtqAvail {
    pub flags: u16,
    pub idx: u16,
    pub ring: [u16; QUEUE_SIZE],
    pub used_event: u16,
}

/// VirtIO used ring element
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VirtqUsedElem {
    pub id: u32,  // Descriptor chain head index
    pub len: u32, // Total bytes written to descriptor buffers
}

/// VirtIO used ring
#[repr(C)]
pub struct VirtqUsed {
    pub flags: u16,
    pub idx: u16,
    pub ring: [VirtqUsedElem; QUEUE_SIZE],
    pub avail_event: u16,
}

/// VirtIO network header (prepended to all packets)
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VirtioNetHdr {
    pub flags: u8,
    pub gso_type: u8,
    pub hdr_len: u16,
    pub gso_size: u16,
    pub csum_start: u16,
    pub csum_offset: u16,
    pub num_buffers: u16,  // Only for mergeable rx buffers
}

impl VirtioNetHdr {
    pub const SIZE: usize = 12; // We use 12 bytes (includes num_buffers for alignment)
}

/// A single virtqueue
pub struct VirtQueue {
    /// Descriptor table
    pub desc: &'static mut [VirtqDesc; QUEUE_SIZE],
    /// Available ring
    pub avail: &'static mut VirtqAvail,
    /// Used ring
    pub used: &'static mut VirtqUsed,
    /// Index of next descriptor to allocate
    free_head: u16,
    /// Number of free descriptors
    num_free: u16,
    /// Last seen used index
    last_used_idx: u16,
    /// Queue index (0 = RX, 1 = TX)
    #[allow(dead_code)]
    queue_idx: u16,
}

impl VirtQueue {
    /// Create a new virtqueue at the given memory address.
    /// Memory must be page-aligned and zeroed.
    pub unsafe fn new(mem: *mut u8, queue_idx: u16) -> Self {
        // Layout: Descriptors | Avail | padding | Used
        let desc = &mut *(mem as *mut [VirtqDesc; QUEUE_SIZE]);
        let avail = &mut *(mem.add(QUEUE_SIZE * 16) as *mut VirtqAvail);
        
        // Used ring must be page-aligned
        let avail_end = mem.add(QUEUE_SIZE * 16 + 6 + 2 * QUEUE_SIZE) as usize;
        let used_start = (avail_end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let used = &mut *(used_start as *mut VirtqUsed);
        
        // Initialize descriptor free list
        for i in 0..QUEUE_SIZE as u16 {
            desc[i as usize].next = i + 1;
        }
        
        VirtQueue {
            desc,
            avail,
            used,
            free_head: 0,
            num_free: QUEUE_SIZE as u16,
            last_used_idx: 0,
            queue_idx,
        }
    }
    
    /// Allocate a descriptor from the free list
    pub fn alloc_desc(&mut self) -> Option<u16> {
        if self.num_free == 0 {
            return None;
        }
        let idx = self.free_head;
        self.free_head = self.desc[idx as usize].next;
        self.num_free -= 1;
        Some(idx)
    }
    
    /// Free a descriptor back to the free list
    pub fn free_desc(&mut self, idx: u16) {
        self.desc[idx as usize].next = self.free_head;
        self.free_head = idx;
        self.num_free += 1;
    }
    
    /// Add a buffer to the available ring
    pub fn push_avail(&mut self, desc_idx: u16) {
        let avail_idx = unsafe { read_volatile(&self.avail.idx) };
        self.avail.ring[(avail_idx as usize) % QUEUE_SIZE] = desc_idx;
        // Memory barrier
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        unsafe { write_volatile(&mut self.avail.idx, avail_idx.wrapping_add(1)) };
    }
    
    /// Check if there are used buffers to process
    #[allow(dead_code)]
    pub fn has_used(&self) -> bool {
        let used_idx = unsafe { read_volatile(&self.used.idx) };
        self.last_used_idx != used_idx
    }
    
    /// Pop a used buffer (returns descriptor index and length)
    pub fn pop_used(&mut self) -> Option<(u16, u32)> {
        let used_idx = unsafe { read_volatile(&self.used.idx) };
        if self.last_used_idx == used_idx {
            return None;
        }
        
        let elem = &self.used.ring[(self.last_used_idx as usize) % QUEUE_SIZE];
        let id = elem.id as u16;
        let len = elem.len;
        
        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        Some((id, len))
    }
}

/// RX buffer entry
struct RxBuffer {
    desc_idx: u16,
    data: [u8; 1526], // Max ethernet frame + virtio header
}

/// TX buffer entry  
struct TxBuffer {
    desc_idx: u16,
    data: [u8; 1526],
}

/// VirtIO Network Driver
pub struct VirtioNet {
    base: usize,
    pub mac: [u8; 6],
    rx_queue: VirtQueue,
    tx_queue: VirtQueue,
    rx_buffers: [Option<RxBuffer>; QUEUE_SIZE],
    tx_buffers: [Option<TxBuffer>; QUEUE_SIZE],
}

// Static storage for queues (must be page-aligned)
#[repr(C, align(4096))]
struct QueueMem {
    data: [u8; PAGE_SIZE * 2],
}

static mut RX_QUEUE_MEM: QueueMem = QueueMem { data: [0; PAGE_SIZE * 2] };
static mut TX_QUEUE_MEM: QueueMem = QueueMem { data: [0; PAGE_SIZE * 2] };

impl VirtioNet {
    /// Read a 32-bit MMIO register
    fn read32(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.base + offset) as *const u32) }
    }
    
    /// Write a 32-bit MMIO register
    fn write32(&self, offset: usize, val: u32) {
        unsafe { write_volatile((self.base + offset) as *mut u32, val) }
    }
    
    /// Read an 8-bit config space register
    fn read_config8(&self, offset: usize) -> u8 {
        unsafe { read_volatile((self.base + CONFIG_SPACE_OFFSET + offset) as *const u8) }
    }
    
    /// Probe for a VirtIO network device by scanning all VirtIO slots.
    /// Returns None if no valid network device found.
    pub fn probe() -> Option<Self> {
        for i in 0..VIRTIO_MAX_DEVICES {
            let addr = VIRTIO_BASE + i * VIRTIO_STRIDE;
            if let Some(dev) = Self::probe_at(addr) {
                return Some(dev);
            }
        }
        None
    }
    
    /// Probe for a VirtIO network device at the given address.
    pub fn probe_at(base: usize) -> Option<Self> {
        let magic = unsafe { read_volatile((base + MAGIC_VALUE_OFFSET) as *const u32) };
        if magic != VIRTIO_MAGIC {
            return None;
        }
        
        let version = unsafe { read_volatile((base + VERSION_OFFSET) as *const u32) };
        if version != VIRTIO_VERSION {
            return None;
        }
        
        let device_id = unsafe { read_volatile((base + DEVICE_ID_OFFSET) as *const u32) };
        if device_id != VIRTIO_NET_DEVICE_ID {
            return None;
        }
        
        // Create uninitialized driver
        let rx_queue = unsafe { VirtQueue::new(RX_QUEUE_MEM.data.as_mut_ptr(), 0) };
        let tx_queue = unsafe { VirtQueue::new(TX_QUEUE_MEM.data.as_mut_ptr(), 1) };
        
        const NONE_RX: Option<RxBuffer> = None;
        const NONE_TX: Option<TxBuffer> = None;
        
        Some(VirtioNet {
            base,
            mac: [0; 6],
            rx_queue,
            tx_queue,
            rx_buffers: [NONE_RX; QUEUE_SIZE],
            tx_buffers: [NONE_TX; QUEUE_SIZE],
        })
    }
    
    /// Get the base address of this device
    pub fn base_addr(&self) -> usize {
        self.base
    }
    
    /// Initialize the device (phase 1: configure queues but don't populate RX buffers yet)
    pub fn init(&mut self) -> Result<(), &'static str> {
        // 1. Reset device
        self.write32(STATUS_OFFSET, 0);
        
        // 2. Set ACKNOWLEDGE status bit
        self.write32(STATUS_OFFSET, STATUS_ACKNOWLEDGE);
        
        // 3. Set DRIVER status bit
        self.write32(STATUS_OFFSET, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
        
        // 4. Read device features
        self.write32(DEVICE_FEATURES_SEL_OFFSET, 0);
        let features = self.read32(DEVICE_FEATURES_OFFSET);
        
        // 5. Negotiate features (we want MAC and STATUS)
        let negotiated = features & (VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS);
        self.write32(DRIVER_FEATURES_SEL_OFFSET, 0);
        self.write32(DRIVER_FEATURES_OFFSET, negotiated);
        
        // 6. Set FEATURES_OK
        self.write32(STATUS_OFFSET, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
        
        // 7. Verify FEATURES_OK is still set
        let status = self.read32(STATUS_OFFSET);
        if status & STATUS_FEATURES_OK == 0 {
            return Err("Device did not accept features");
        }
        
        // 8. Set page size (legacy)
        self.write32(GUEST_PAGE_SIZE_OFFSET, PAGE_SIZE as u32);
        
        // 9. Configure RX queue (queue 0)
        self.write32(QUEUE_SEL_OFFSET, 0);
        let queue_max = self.read32(QUEUE_NUM_MAX_OFFSET);
        if queue_max < QUEUE_SIZE as u32 {
            return Err("RX queue too small");
        }
        self.write32(QUEUE_NUM_OFFSET, QUEUE_SIZE as u32);
        
        // Calculate PFN (page frame number) for RX queue
        let rx_pfn = unsafe { RX_QUEUE_MEM.data.as_ptr() as u64 / PAGE_SIZE as u64 };
        self.write32(QUEUE_PFN_OFFSET, rx_pfn as u32);
        
        // 10. Configure TX queue (queue 1)
        self.write32(QUEUE_SEL_OFFSET, 1);
        let queue_max = self.read32(QUEUE_NUM_MAX_OFFSET);
        if queue_max < QUEUE_SIZE as u32 {
            return Err("TX queue too small");
        }
        self.write32(QUEUE_NUM_OFFSET, QUEUE_SIZE as u32);
        
        let tx_pfn = unsafe { TX_QUEUE_MEM.data.as_ptr() as u64 / PAGE_SIZE as u64 };
        self.write32(QUEUE_PFN_OFFSET, tx_pfn as u32);
        
        // 11. Read MAC address from config space
        for i in 0..6 {
            self.mac[i] = self.read_config8(i);
        }
        
        // 12. Set DRIVER_OK
        self.write32(STATUS_OFFSET, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);
        
        // NOTE: RX queue population must happen AFTER the VirtioNet is moved to its final location!
        // Call finalize_init() after the containing struct is in place.
        
        Ok(())
    }
    
    /// Finalize initialization by populating RX buffers.
    /// Must be called AFTER the VirtioNet struct is in its final memory location!
    pub fn finalize_init(&mut self) {
        self.populate_rx_queue();
    }
    
    /// Populate the RX queue with empty buffers
    fn populate_rx_queue(&mut self) {
        for i in 0..QUEUE_SIZE {
            if self.rx_buffers[i].is_some() {
                continue;
            }
            
            let desc_idx = match self.rx_queue.alloc_desc() {
                Some(idx) => idx,
                None => break,
            };
            
            // Create and store buffer FIRST
            self.rx_buffers[i] = Some(RxBuffer {
                desc_idx,
                data: [0; 1526],
            });
            
            // Now get the address from the stored buffer (after it's been placed in its final location)
            let buffer = self.rx_buffers[i].as_ref().unwrap();
            
            // Set up descriptor (device writes to this buffer)
            let desc = &mut self.rx_queue.desc[desc_idx as usize];
            desc.addr = buffer.data.as_ptr() as u64;
            desc.len = buffer.data.len() as u32;
            desc.flags = VRING_DESC_F_WRITE;
            desc.next = 0;
            
            // Add to available ring
            self.rx_queue.push_avail(desc_idx);
        }
        
        // Notify device that RX buffers are available
        self.write32(QUEUE_NOTIFY_OFFSET, 0);
    }
    
    /// Receive a packet (returns None if no packet available)
    #[allow(dead_code)]
    pub fn recv(&mut self) -> Option<&[u8]> {
        // Check for used buffers
        let (desc_idx, total_len) = self.rx_queue.pop_used()?;
        
        // Find the buffer
        for buf_opt in &self.rx_buffers {
            if let Some(buf) = buf_opt {
                if buf.desc_idx == desc_idx {
                    // Skip virtio header (12 bytes)
                    let data_start = VirtioNetHdr::SIZE;
                    let data_len = total_len as usize - VirtioNetHdr::SIZE;
                    if data_len > 0 && data_start + data_len <= buf.data.len() {
                        return Some(&buf.data[data_start..data_start + data_len]);
                    }
                }
            }
        }
        None
    }
    
    /// Recycle an RX buffer after processing
    pub fn recycle_rx(&mut self, desc_idx: u16) {
        // Re-add to available ring
        self.rx_queue.push_avail(desc_idx);
        // Notify device
        self.write32(QUEUE_NOTIFY_OFFSET, 0);
    }
    
    /// Receive a packet with full control (returns desc_idx for recycling)
    pub fn recv_with_desc(&mut self) -> Option<(u16, &[u8])> {
        let (desc_idx, total_len) = self.rx_queue.pop_used()?;
        
        for buf_opt in &self.rx_buffers {
            if let Some(buf) = buf_opt {
                if buf.desc_idx == desc_idx {
                    let data_start = VirtioNetHdr::SIZE;
                    let data_len = (total_len as usize).saturating_sub(VirtioNetHdr::SIZE);
                    if data_len > 0 && data_start + data_len <= buf.data.len() {
                        return Some((desc_idx, &buf.data[data_start..data_start + data_len]));
                    }
                }
            }
        }
        None
    }
    
    /// Send a packet
    pub fn send(&mut self, data: &[u8]) -> Result<(), &'static str> {
        if data.len() > 1514 {
            return Err("Packet too large");
        }
        
        // Allocate descriptor
        let desc_idx = self.tx_queue.alloc_desc().ok_or("No TX descriptors available")?;
        
        // Find free TX buffer slot
        let mut slot_idx = None;
        for (i, buf_opt) in self.tx_buffers.iter().enumerate() {
            if buf_opt.is_none() {
                slot_idx = Some(i);
                break;
            }
        }
        let slot_idx = slot_idx.ok_or("No TX buffer slots")?;
        
        // Create buffer with virtio header + data
        let mut buffer = TxBuffer {
            desc_idx,
            data: [0; 1526],
        };
        
        // Write virtio header (all zeros)
        // Then copy packet data
        buffer.data[VirtioNetHdr::SIZE..VirtioNetHdr::SIZE + data.len()].copy_from_slice(data);
        
        // Set up descriptor
        let desc = &mut self.tx_queue.desc[desc_idx as usize];
        desc.addr = buffer.data.as_ptr() as u64;
        desc.len = (VirtioNetHdr::SIZE + data.len()) as u32;
        desc.flags = 0; // Device reads from this buffer
        desc.next = 0;
        
        self.tx_buffers[slot_idx] = Some(buffer);
        
        // Add to available ring
        self.tx_queue.push_avail(desc_idx);
        
        // Notify device
        self.write32(QUEUE_NOTIFY_OFFSET, 1);
        
        Ok(())
    }
    
    /// Process completed TX buffers
    pub fn process_tx(&mut self) {
        while let Some((desc_idx, _len)) = self.tx_queue.pop_used() {
            // Find and free the buffer
            for buf_opt in &mut self.tx_buffers {
                if let Some(buf) = buf_opt {
                    if buf.desc_idx == desc_idx {
                        *buf_opt = None;
                        break;
                    }
                }
            }
            // Return descriptor to free list
            self.tx_queue.free_desc(desc_idx);
        }
    }
    
    /// Poll for activity (call periodically)
    pub fn poll(&mut self) {
        // Process completed TX buffers
        self.process_tx();
        
        // Acknowledge interrupts
        let status = self.read32(INTERRUPT_STATUS_OFFSET);
        if status != 0 {
            self.write32(INTERRUPT_ACK_OFFSET, status);
        }
    }
    
    /// Check if the device has an interrupt pending
    #[allow(dead_code)]
    pub fn has_interrupt(&self) -> bool {
        self.read32(INTERRUPT_STATUS_OFFSET) != 0
    }
    
    /// Get MAC address as a formatted string
    pub fn mac_str(&self) -> [u8; 17] {
        let mut buf = [0u8; 17];
        let hex = b"0123456789abcdef";
        for i in 0..6 {
            buf[i * 3] = hex[(self.mac[i] >> 4) as usize];
            buf[i * 3 + 1] = hex[(self.mac[i] & 0xf) as usize];
            if i < 5 {
                buf[i * 3 + 2] = b':';
            }
        }
        buf
    }
    
    /// Read the IP address from the device configuration space.
    /// This is a custom extension (Config offset 8 = 0x108 absolute).
    /// Returns None if the IP is 0.0.0.0 (not yet assigned).
    pub fn get_config_ip(&self) -> Option<[u8; 4]> {
        // Read 32-bit value at config offset 8 (CONFIG_SPACE_OFFSET + 8)
        let ip_u32 = unsafe { 
            read_volatile((self.base + CONFIG_SPACE_OFFSET + 8) as *const u32) 
        };
        if ip_u32 == 0 {
            None
        } else {
            Some(ip_u32.to_le_bytes())
        }
    }
}

