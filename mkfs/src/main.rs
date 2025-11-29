use clap::Parser;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

const SECTOR_SIZE: u64 = 512;
const MAGIC: u32 = 0x53465331; // "SFS1"

// Layout
const SEC_SUPER: u64 = 0;
const SEC_MAP_START: u64 = 1;
const SEC_MAP_COUNT: u64 = 64; // Covers ~128MB
const SEC_DIR_START: u64 = 65;
const SEC_DIR_COUNT: u64 = 64; // 1024 files max
const SEC_DATA_START: u64 = 129;

#[derive(Parser)]
struct Args {
    /// Output disk image path
    #[arg(short, long)]
    output: PathBuf,

    /// Directory to import files from
    #[arg(short, long)]
    dir: Option<PathBuf>,

    /// Disk size in MB
    #[arg(short, long, default_value_t = 128)]
    size: u64,
}

#[repr(C, packed)]
struct DirEntry {
    name: [u8; 24],
    size: u32,
    head: u32,
}

fn main() -> std::io::Result<()> {
    let args = Args::parse();
    
    let total_sectors = (args.size * 1024 * 1024) / SECTOR_SIZE;
    println!("Creating SFS image: {:?} ({} MB, {} sectors)", args.output, args.size, total_sectors);

    let mut file = File::create(&args.output)?;
    file.set_len(args.size * 1024 * 1024)?;

    // 1. Write Superblock
    file.seek(SeekFrom::Start(SEC_SUPER * SECTOR_SIZE))?;
    file.write_all(&MAGIC.to_le_bytes())?;
    file.write_all(&(total_sectors as u32).to_le_bytes())?;

    // 2. Initialize Bitmap (Mark system sectors as used)
    let mut bitmap = vec![0u8; (SEC_MAP_COUNT * SECTOR_SIZE) as usize];
    let reserved_sectors = SEC_DATA_START;
    for i in 0..reserved_sectors {
        let byte_idx = (i / 8) as usize;
        let bit_idx = i % 8;
        if byte_idx < bitmap.len() {
            bitmap[byte_idx] |= 1 << bit_idx;
        }
    }
    
    // 3. Import Files
    if let Some(src_dir) = args.dir {
        if src_dir.exists() {
            let mut dir_idx = 0;
            for entry in fs::read_dir(src_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_file() {
                    let filename = path.file_name().unwrap().to_str().unwrap();
                    if filename.len() > 23 {
                        println!("Skipping {}: Name too long (max 23 chars)", filename);
                        continue;
                    }
                    println!("Importing {}", filename);
                    
                    let data = fs::read(&path)?;
                    let head_sector = write_data(&mut file, &mut bitmap, &data)?;
                    write_dir_entry(&mut file, dir_idx, filename, data.len() as u32, head_sector)?;
                    dir_idx += 1;
                }
            }
        }
    }

    // 4. Write Bitmap back to disk
    file.seek(SeekFrom::Start(SEC_MAP_START * SECTOR_SIZE))?;
    file.write_all(&bitmap)?;

    println!("Done.");
    Ok(())
}

fn find_free_sector(bitmap: &mut [u8]) -> Option<u32> {
    for (byte_idx, &byte) in bitmap.iter().enumerate() {
        if byte != 0xFF {
            for bit_idx in 0..8 {
                if (byte & (1 << bit_idx)) == 0 {
                    bitmap[byte_idx] |= 1 << bit_idx;
                    return Some((byte_idx * 8 + bit_idx) as u32);
                }
            }
        }
    }
    None
}

fn write_data(file: &mut File, bitmap: &mut [u8], data: &[u8]) -> std::io::Result<u32> {
    if data.is_empty() { return Ok(0); }

    let mut remaining = data;
    let head = find_free_sector(bitmap).expect("Disk full");
    let mut current = head;

    while !remaining.is_empty() {
        let chunk_len = std::cmp::min(remaining.len(), 508);
        let chunk = &remaining[..chunk_len];
        remaining = &remaining[chunk_len..];

        let next = if remaining.is_empty() { 0 } else { find_free_sector(bitmap).expect("Disk full") };

        file.seek(SeekFrom::Start(current as u64 * SECTOR_SIZE))?;
        file.write_all(&next.to_le_bytes())?;
        file.write_all(chunk)?;
        // Pad with zeros if partial sector
        if chunk_len < 508 {
            file.write_all(&vec![0u8; 508 - chunk_len])?;
        }

        current = next;
    }
    Ok(head)
}

fn write_dir_entry(file: &mut File, idx: u64, name: &str, size: u32, head: u32) -> std::io::Result<()> {
    let offset = (SEC_DIR_START * SECTOR_SIZE) + (idx * 32);
    file.seek(SeekFrom::Start(offset))?;

    let mut name_bytes = [0u8; 24];
    let nb = name.as_bytes();
    name_bytes[..nb.len()].copy_from_slice(nb);

    file.write_all(&name_bytes)?;
    file.write_all(&size.to_le_bytes())?;
    file.write_all(&head.to_le_bytes())?;
    Ok(())
}