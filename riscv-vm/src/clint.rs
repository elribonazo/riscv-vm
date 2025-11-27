pub const CLINT_BASE: u64 = 0x0200_0000;
pub const CLINT_SIZE: u64 = 0x10000;

pub const MSIP_OFFSET: u64 = 0x0000;
pub const MTIME_OFFSET: u64 = 0xbff8;
pub const MTIMECMP_OFFSET: u64 = 0x4000;

pub const MAX_HARTS: usize = 8;

/// Time increment per CPU step (in timer ticks).
/// At 10MHz and ~1 instruction per cycle at ~10MHz CPU, this gives roughly real-time.
/// Adjust for desired timer granularity.
const MTIME_INCREMENT: u64 = 1;

pub struct Clint {
    pub msip: [u32; MAX_HARTS],
    pub mtimecmp: [u64; MAX_HARTS],
    /// Machine timer counter. Incremented by `tick()` each CPU step.
    pub mtime: u64,
    pub debug: bool,
}

impl Clint {
    pub fn new() -> Self {
        Self {
            msip: [0; MAX_HARTS],
            mtimecmp: [u64::MAX; MAX_HARTS], 
            mtime: 0,
            debug: false,
        }
    }

    /// Returns the current mtime value.
    pub fn mtime(&self) -> u64 {
        self.mtime
    }

    /// Sets mtime to a specific value (used for snapshot restore).
    pub fn set_mtime(&mut self, val: u64) {
        self.mtime = val;
    }

    /// Advance mtime by one tick. Called once per CPU step.
    pub fn tick(&mut self) {
        self.mtime = self.mtime.wrapping_add(MTIME_INCREMENT);
    }

    /// Backward compatibility: increment is now tick()
    pub fn increment(&mut self) {
        self.tick();
    }

    pub fn sync_time_micros(&mut self, _micros: u64) {
        // No-op for deterministic timer
    }

    /// Load from the CLINT register space.
    ///
    /// Offsets are relative to `CLINT_BASE`. Only naturally aligned 4- and
    /// 8-byte accesses are architecturally meaningful; other sizes return 0.
    pub fn load(&self, offset: u64, size: u64) -> u64 {
        match (offset, size) {
            // MSIP[hart], 32-bit
            (o, 4) if o >= MSIP_OFFSET && o < MSIP_OFFSET + (MAX_HARTS as u64 * 4) => {
                let hart_idx = ((o - MSIP_OFFSET) / 4) as usize;
                self.msip[hart_idx] as u64
            }

            // MTIME, 64-bit
            (MTIME_OFFSET, 8) => self.mtime(),
            // MTIME, low/high 32-bit words
            (MTIME_OFFSET, 4) => self.mtime() & 0xffff_ffff,
            (o, 4) if o == MTIME_OFFSET + 4 => self.mtime() >> 32,

            // MTIMECMP[hart], 64-bit and split 32-bit accesses
            (o, 8) if o >= MTIMECMP_OFFSET && o < MTIMECMP_OFFSET + (MAX_HARTS as u64 * 8) => {
                let hart_idx = ((o - MTIMECMP_OFFSET) / 8) as usize;
                self.mtimecmp[hart_idx]
            }
            (o, 4) if o >= MTIMECMP_OFFSET && o < MTIMECMP_OFFSET + (MAX_HARTS as u64 * 8) => {
                let hart_idx = ((o - MTIMECMP_OFFSET) / 8) as usize;
                let sub = (o - MTIMECMP_OFFSET) % 8;
                let val = self.mtimecmp[hart_idx];
                match sub {
                    0 => val & 0xffff_ffff,
                    4 => val >> 32,
                    _ => 0,
                }
            }

            // Other offsets/sizes are reserved -> read as zero.
            _ => 0,
        }
    }

    /// Store into the CLINT register space.
    ///
    /// Offsets are relative to `CLINT_BASE`. Mis-sized or strange offsets are
    /// ignored to keep the device side-effect free for unsupported accesses.
    pub fn store(&mut self, offset: u64, size: u64, value: u64) {
        match (offset, size) {
            // MSIP[hart], 32-bit
            (o, 4) if o >= MSIP_OFFSET && o < MSIP_OFFSET + (MAX_HARTS as u64 * 4) => {
                let hart_idx = ((o - MSIP_OFFSET) / 4) as usize;
                // Only the LSB matters for MSIP
                self.msip[hart_idx] = (value & 1) as u32;
            }

            // MTIME is read-only in this implementation (driven by wall clock)
            (MTIME_OFFSET, _) => {}
            (o, 4) if o == MTIME_OFFSET + 4 => {}

            // MTIMECMP[hart], 64-bit and split 32-bit writes
            (o, 8) if o >= MTIMECMP_OFFSET && o < MTIMECMP_OFFSET + (MAX_HARTS as u64 * 8) => {
                let hart_idx = ((o - MTIMECMP_OFFSET) / 8) as usize;
                self.mtimecmp[hart_idx] = value;
            }
            (o, 4) if o >= MTIMECMP_OFFSET && o < MTIMECMP_OFFSET + (MAX_HARTS as u64 * 8) => {
                let hart_idx = ((o - MTIMECMP_OFFSET) / 8) as usize;
                let sub = (o - MTIMECMP_OFFSET) % 8;
                let current = self.mtimecmp[hart_idx];
                self.mtimecmp[hart_idx] = match sub {
                    0 => (current & 0xffff_ffff_0000_0000) | (value & 0xffff_ffff),
                    4 => (current & 0x0000_0000_ffff_ffff) | (value << 32),
                    _ => current,
                };
            }

            _ => {}
        }
    }
}
