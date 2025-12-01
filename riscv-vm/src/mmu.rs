use crate::bus::Bus;
use crate::csr::Mode;
use crate::Trap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AccessType {
    Instruction,
    Load,
    Store,
}

const PAGE_SIZE: u64 = 4096;
const PTE_SIZE: u64 = 8;
const MAX_LEVELS: usize = 4;

/// TLB size (power of 2 for fast modulo)
const TLB_SIZE: usize = 64;
const TLB_MASK: usize = TLB_SIZE - 1;

/// Permission bit masks for packed perm field
pub const PERM_R: u8 = 1 << 0;
pub const PERM_W: u8 = 1 << 1;
pub const PERM_X: u8 = 1 << 2;
pub const PERM_U: u8 = 1 << 3;
pub const PERM_A: u8 = 1 << 4;
pub const PERM_D: u8 = 1 << 5;
pub const PERM_G: u8 = 1 << 6; // Global mapping bit

/// Compact, cache-friendly TLB entry structure.
/// Memory layout: 8 + 8 + 2 + 1 + 1 + 1 + padding = 24 bytes
/// Total TLB: 64 × 24 = 1.5KB (fits in L1 cache)
#[derive(Clone, Copy, Debug)]
pub struct TlbEntry {
    /// Virtual page number (upper bits of VA)
    pub vpn: u64,
    /// Physical page number (translated)
    pub ppn: u64,
    /// Address Space Identifier (16 bits in RISC-V SATP)
    pub asid: u16,
    /// Packed permission bits (R/W/X/U/A/D/G)
    pub perm: u8,
    /// Page level (0=4KB, 1=2MB megapage, 2=1GB gigapage, 3=512GB)
    pub level: u8,
    /// Entry is valid
    pub valid: bool,
}

impl TlbEntry {
    /// Empty/invalid TLB entry constant
    pub const EMPTY: Self = Self {
        vpn: 0,
        ppn: 0,
        asid: 0,
        perm: 0,
        level: 0,
        valid: false,
    };

    /// Check if entry has read permission
    #[inline(always)]
    pub const fn r(&self) -> bool {
        self.perm & PERM_R != 0
    }

    /// Check if entry has write permission
    #[inline(always)]
    pub const fn w(&self) -> bool {
        self.perm & PERM_W != 0
    }

    /// Check if entry has execute permission
    #[inline(always)]
    pub const fn x(&self) -> bool {
        self.perm & PERM_X != 0
    }

    /// Check if entry is user-accessible
    #[inline(always)]
    pub const fn u(&self) -> bool {
        self.perm & PERM_U != 0
    }

    /// Check if entry has been accessed
    #[inline(always)]
    pub const fn a(&self) -> bool {
        self.perm & PERM_A != 0
    }

    /// Check if entry is dirty (written)
    #[inline(always)]
    pub const fn d(&self) -> bool {
        self.perm & PERM_D != 0
    }

    /// Check if entry is global (ignores ASID)
    #[inline(always)]
    pub const fn global(&self) -> bool {
        self.perm & PERM_G != 0
    }

    /// Set the accessed bit
    #[inline(always)]
    pub fn set_a(&mut self) {
        self.perm |= PERM_A;
    }

    /// Set the dirty bit
    #[inline(always)]
    pub fn set_d(&mut self) {
        self.perm |= PERM_D;
    }
}

impl Default for TlbEntry {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// Direct-mapped TLB for fast virtual-to-physical address translation
pub struct Tlb {
    entries: [TlbEntry; TLB_SIZE],
}

impl Tlb {
    pub fn new() -> Self {
        Self {
            entries: [TlbEntry::EMPTY; TLB_SIZE],
        }
    }

    /// Flush entire TLB (SFENCE.VMA with rs1=x0, rs2=x0)
    #[inline]
    pub fn flush(&mut self) {
        for entry in &mut self.entries {
            entry.valid = false;
        }
    }

    /// Flush by ASID (SFENCE.VMA with rs2!=x0)
    /// Global mappings are not flushed.
    #[inline]
    pub fn flush_asid(&mut self, asid: u64) {
        let asid16 = asid as u16;
        for entry in &mut self.entries {
            if !entry.global() && entry.asid == asid16 {
                entry.valid = false;
            }
        }
    }

    /// Flush specific virtual address (SFENCE.VMA with rs1!=x0)
    #[inline]
    pub fn flush_va(&mut self, va: u64) {
        let vpn = va >> 12;
        let idx = (vpn as usize) & TLB_MASK;
        // SAFETY: idx is always < TLB_SIZE due to the bitmask
        let entry = unsafe { self.entries.get_unchecked_mut(idx) };

        if entry.vpn == vpn {
            entry.valid = false;
        }
    }

    /// Flush specific page with ASID check (SFENCE.VMA with rs1!=x0, rs2!=x0)
    #[inline]
    pub fn flush_page(&mut self, vpn: u64, asid: u64) {
        let idx = (vpn as usize) & TLB_MASK;
        // SAFETY: idx is always < TLB_SIZE due to the bitmask
        let entry = unsafe { self.entries.get_unchecked_mut(idx) };

        if entry.valid && entry.vpn == vpn {
            // For page-specific flush, invalidate matching ASID mappings.
            // Global mappings ignore ASID and are treated as matching.
            let match_asid = entry.global() || entry.asid == asid as u16;
            if match_asid {
                entry.valid = false;
            }
        }
    }

    /// Look up a virtual page number in the TLB.
    /// Returns reference to TlbEntry if found, None if miss.
    #[inline(always)]
    pub fn lookup(&self, vpn: u64, asid: u64) -> Option<&TlbEntry> {
        let idx = (vpn as usize) & TLB_MASK;
        // SAFETY: idx is always < TLB_SIZE due to the bitmask
        let entry = unsafe { self.entries.get_unchecked(idx) };

        // Hit if: valid AND VPN matches AND (entry is global OR ASID matches).
        if entry.valid && entry.vpn == vpn && (entry.global() || entry.asid == asid as u16) {
            Some(entry)
        } else {
            None
        }
    }

    /// Fast lookup returning just (ppn, perm) for common case
    #[inline(always)]
    pub fn lookup_fast(&self, vpn: u64, asid: u64) -> Option<(u64, u8)> {
        let idx = (vpn as usize) & TLB_MASK;
        // SAFETY: idx is always < TLB_SIZE due to the bitmask
        let entry = unsafe { self.entries.get_unchecked(idx) };

        if entry.valid && entry.vpn == vpn && (entry.global() || entry.asid == asid as u16) {
            Some((entry.ppn, entry.perm))
        } else {
            None
        }
    }

    /// Look up with level awareness (for superpages)
    #[inline]
    pub fn lookup_with_level(&self, vpn: u64, asid: u64) -> Option<(u64, u8, u8)> {
        let idx = (vpn as usize) & TLB_MASK;
        // SAFETY: idx is always < TLB_SIZE due to the bitmask
        let entry = unsafe { self.entries.get_unchecked(idx) };

        if entry.valid && entry.vpn == vpn && (entry.global() || entry.asid == asid as u16) {
            Some((entry.ppn, entry.perm, entry.level))
        } else {
            None
        }
    }

    /// Insert a TLB entry. Overwrites any existing entry at the same index.
    #[inline(always)]
    pub fn insert(&mut self, entry: TlbEntry) {
        let idx = (entry.vpn as usize) & TLB_MASK;
        // SAFETY: idx is always < TLB_SIZE due to the bitmask
        unsafe {
            *self.entries.get_unchecked_mut(idx) = entry;
        }
    }

    /// Insert a translation with explicit parameters.
    /// Overwrites any existing entry at the same index (direct-mapped).
    #[inline]
    pub fn insert_translation(
        &mut self,
        vpn: u64,
        ppn: u64,
        perm: u8,
        level: u8,
        asid: u16,
    ) {
        let idx = (vpn as usize) & TLB_MASK;
        // SAFETY: idx is always < TLB_SIZE due to the bitmask
        unsafe {
            *self.entries.get_unchecked_mut(idx) = TlbEntry {
                vpn,
                ppn,
                asid,
                perm,
                level,
                valid: true,
            };
        }
    }
}

/// Sv39/Sv48 translation + A/D bit updates.
///
/// `addr` is a virtual address. Returns the translated physical address or a
/// `Trap` corresponding to the appropriate page/access fault.
pub fn translate(
    bus: &dyn Bus,
    tlb: &mut Tlb,
    mode: Mode,
    satp: u64,
    mstatus: u64,
    addr: u64,
    access_type: AccessType,
) -> Result<u64, Trap> {
    // No translation in Machine mode (always Bare).
    if mode == Mode::Machine {
        return Ok(addr);
    }

    let satp_mode = (satp >> 60) & 0xF;
    let current_asid = (satp >> 44) & 0xFFFF;

    let (levels, va_bits, vpn_full_mask): (usize, u64, u64) = match satp_mode {
        0 => {
            // Bare: no translation.
            return Ok(addr);
        }
        8 => {
            // Sv39
            let levels = 3;
            let va_bits = 39;
            let vpn_full_mask = (1u64 << (9 * levels)) - 1;
            (levels, va_bits, vpn_full_mask)
        }
        9 => {
            // Sv48 (supported by this MMU, though not required for virt).
            let levels = 4;
            let va_bits = 48;
            let vpn_full_mask = (1u64 << (9 * levels)) - 1;
            (levels, va_bits, vpn_full_mask)
        }
        _ => {
            // Unsupported mode: treat as Bare.
            return Ok(addr);
        }
    };

    // Check canonical form of the virtual address for the configured VA width.
    let sign_bit = va_bits - 1;
    let upper_mask = !((1u64 << va_bits) - 1);
    let sign = (addr >> sign_bit) & 1;
    let expected_upper = if sign == 1 { upper_mask } else { 0 };
    if (addr & upper_mask) != expected_upper {
        return Err(page_fault(access_type, addr));
    }

    let vpn_full = (addr >> 12) & vpn_full_mask;

    // TLB hit path.
    if let Some(entry) = tlb.lookup(vpn_full, current_asid) {
        if check_permission_tlb(mode, mstatus, entry, access_type) {
            // For now we do not lazily update A/D on TLB hits – page table
            // entries are already marked by the walk that inserted this entry.
            let offset = addr & 0xFFF;
            let pa = (entry.ppn << 12) | offset;
            return Ok(pa);
        } else {
            return Err(page_fault(access_type, addr));
        }
    }

    // Page table walk on TLB miss.
    let mut vpn = [0u64; MAX_LEVELS];
    for level in 0..levels {
        vpn[level] = (addr >> (12 + 9 * level as u64)) & 0x1FF;
    }

    let root_ppn = satp & ((1u64 << 44) - 1);
    let mut a = root_ppn * PAGE_SIZE;

    for i in (0..levels).rev() {
        let pte_addr = a + vpn[i] * PTE_SIZE;

        let pte = match bus.load(pte_addr, 8) {
            Ok(val) => val,
            Err(_) => return Err(access_fault(access_type, addr)),
        };

        let v = (pte >> 0) & 1;
        let r = (pte >> 1) & 1;
        let w = (pte >> 2) & 1;
        let x = (pte >> 3) & 1;

        // Invalid or malformed.
        if v == 0 || (r == 0 && w == 1) {
            return Err(page_fault(access_type, addr));
        }

        // Pointer to next level if R=X=0.
        if r == 0 && x == 0 {
            if i == 0 {
                return Err(page_fault(access_type, addr));
            }
            let ppn = (pte >> 10) & 0xFFF_FFFF_FFFF;
            a = ppn * PAGE_SIZE;
            continue;
        }

        // Leaf PTE - extract permission bits into packed format
        let mut perm: u8 = 0;
        if r != 0 {
            perm |= PERM_R;
        }
        if w != 0 {
            perm |= PERM_W;
        }
        if x != 0 {
            perm |= PERM_X;
        }
        if (pte >> 4) & 1 != 0 {
            perm |= PERM_U;
        }
        if (pte >> 5) & 1 != 0 {
            perm |= PERM_G;
        }
        if (pte >> 6) & 1 != 0 {
            perm |= PERM_A;
        }
        if (pte >> 7) & 1 != 0 {
            perm |= PERM_D;
        }

        let mut entry = TlbEntry {
            vpn: vpn_full,
            ppn: (pte >> 10) & 0xFFF_FFFF_FFFF,
            asid: current_asid as u16,
            perm,
            level: i as u8,
            valid: true,
        };

        if !check_permission_tlb(mode, mstatus, &entry, access_type) {
            return Err(page_fault(access_type, addr));
        }

        // Superpage alignment checks (Sv39/48 spec).
        if i > 0 {
            let ppn_mask = (1 << (9 * i)) - 1;
            let ppn = (pte >> 10) & 0xFFF_FFFF_FFFF;
            if (ppn & ppn_mask) != 0 {
                return Err(page_fault(access_type, addr));
            }
        }

        // A/D bit updates: set in memory and in the cached entry.
        let mut new_pte = pte;
        let mut update = false;

        if !entry.a() {
            new_pte |= 1 << 6;
            entry.set_a();
            update = true;
        }
        if matches!(access_type, AccessType::Store) && !entry.d() {
            new_pte |= 1 << 7;
            entry.set_d();
            update = true;
        }

        if update {
            if bus.store(pte_addr, 8, new_pte).is_err() {
                return Err(access_fault(access_type, addr));
            }
        }

        let offset_in_page = addr & 0xFFF;

        // Construct final PPN, filling low parts from the VA on superpages.
        let ppn = (pte >> 10) & 0xFFF_FFFF_FFFF;
        let vpn_mask = (1 << (9 * i)) - 1;
        let result_ppn = (ppn & !vpn_mask) | ((addr >> 12) & vpn_mask);

        entry.ppn = result_ppn;
        tlb.insert(entry);

        let pa = (result_ppn << 12) | offset_in_page;
        return Ok(pa);
    }

    Err(page_fault(access_type, addr))
}

#[inline(always)]
fn check_permission_tlb(mode: Mode, mstatus: u64, entry: &TlbEntry, access_type: AccessType) -> bool {
    let mxr = (mstatus >> 19) & 1;
    let sum = (mstatus >> 18) & 1;

    match mode {
        Mode::Supervisor => {
            if entry.u() {
                if matches!(access_type, AccessType::Instruction) {
                    return false;
                }
                if sum == 0 {
                    return false;
                }
            }
        }
        Mode::User => {
            if !entry.u() {
                return false;
            }
        }
        Mode::Machine => {}
    }

    match access_type {
        AccessType::Instruction => entry.x(),
        AccessType::Store => entry.w(),
        AccessType::Load => {
            if entry.r() {
                true
            } else {
                mxr == 1 && entry.x()
            }
        }
    }
}

#[inline]
fn page_fault(access_type: AccessType, addr: u64) -> Trap {
    match access_type {
        AccessType::Instruction => Trap::InstructionPageFault(addr),
        AccessType::Load => Trap::LoadPageFault(addr),
        AccessType::Store => Trap::StorePageFault(addr),
    }
}

#[inline]
fn access_fault(access_type: AccessType, addr: u64) -> Trap {
    match access_type {
        AccessType::Instruction => Trap::InstructionAccessFault(addr),
        AccessType::Load => Trap::LoadAccessFault(addr),
        AccessType::Store => Trap::StoreAccessFault(addr),
    }
}
