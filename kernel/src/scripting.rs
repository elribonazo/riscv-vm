// kernel/src/scripting.rs
//! Script discovery for WASM binaries in /usr/bin/
//!
//! This module provides script lookup functionality for the shell.
//! Scripts are WASM binaries located in /usr/bin/ directory.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Find a script/binary by name
/// 
/// Search order:
/// 1. If path contains '/', resolve as absolute or relative path
/// 2. Search /usr/bin/<name>
/// 3. Search root /<name>
pub fn find_script(cmd: &str) -> Option<Vec<u8>> {
    let fs_guard = crate::FS_STATE.lock();
    let mut blk_guard = crate::BLK_DEV.lock();

    if let (Some(fs), Some(dev)) = (fs_guard.as_ref(), blk_guard.as_mut()) {
        // If command contains '/', treat as path
        if cmd.contains('/') {
            let full_path = if cmd.starts_with('/') {
                String::from(cmd)
            } else {
                crate::resolve_path(cmd)
            };

            if let Some(content) = fs.read_file(dev, &full_path) {
                return Some(content);
            }
            return None;
        }

        // Search /usr/bin/ first
        let usr_bin_path = format!("/usr/bin/{}", cmd);
        if let Some(content) = fs.read_file(dev, &usr_bin_path) {
            return Some(content);
        }

        // Search root as fallback
        if let Some(content) = fs.read_file(dev, cmd) {
            return Some(content);
        }
    }

    None
}
