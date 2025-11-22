use crate::bus::{Bus, DRAM_BASE};
use elf::{ElfBytes, abi::PT_LOAD, endian::LittleEndian, segment::ProgramHeader};

pub fn load_image(bus: &mut Bus, image: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    if image.starts_with(b"\x7fELF") {
        load_elf_segments(bus, image)?;
    } else {
        bus.write_bytes(DRAM_BASE, image)?;
    }

    Ok(())
}

fn load_elf_segments(bus: &mut Bus, image: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let elf = ElfBytes::<LittleEndian>::minimal_parse(image)?;
    let segments = elf
        .segments()
        .ok_or_else(|| "ELF file is missing program headers")?;

    for phdr in segments.iter() {
        if phdr.p_type != PT_LOAD {
            continue;
        }

        let (start, end) = segment_file_range(&phdr)?;
        let segment = &image[start..end];
        let load_addr = if phdr.p_paddr != 0 {
            phdr.p_paddr
        } else {
            phdr.p_vaddr
        };

        if !segment.is_empty() {
            bus.write_bytes(load_addr, segment)?;
        }

        if phdr.p_memsz > phdr.p_filesz {
            let zero_len = (phdr.p_memsz - phdr.p_filesz) as usize;
            let zero_base = load_addr + phdr.p_filesz;
            bus.fill_bytes(zero_base, zero_len, 0)?;
        }
    }

    Ok(())
}

fn segment_file_range(phdr: &ProgramHeader) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    let start = usize::try_from(phdr.p_offset)?;
    let size = usize::try_from(phdr.p_filesz)?;
    let end = start
        .checked_add(size)
        .ok_or("ELF segment range overflow")?;
    Ok((start, end))
}
