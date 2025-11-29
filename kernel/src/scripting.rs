// kernel/src/scripting.rs
//! JavaScript-like scripting runtime with ES6 module system
//!
//! Scripts use `import * from` to import OS modules:
//!   import * from "os:fs"
//!   import * from "os:net"
//!   import * from "os:sys"
//!   import * from "os:mem"

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::format;
use rhai::{Engine, Scope, Dynamic, Array, Map, ImmutableString, packages::{Package, StandardPackage}};

// ═══════════════════════════════════════════════════════════════════════════════
// MODULE TYPES - For namespace imports (import * as X from "...")
// ═══════════════════════════════════════════════════════════════════════════════

/// Filesystem module object - os:fs
#[derive(Clone)]
pub struct FsModule;

/// Network module object - os:net
#[derive(Clone)]
pub struct NetModule;

/// System module object - os:sys
#[derive(Clone)]
pub struct SysModule;

/// Memory module object - os:mem
#[derive(Clone)]
pub struct MemModule;


// ═══════════════════════════════════════════════════════════════════════════════
// LOGGING
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Off => "OFF",
            LogLevel::Error => "ERROR",
            LogLevel::Warn => "WARN",
            LogLevel::Info => "INFO",
            LogLevel::Debug => "DEBUG",
            LogLevel::Trace => "TRACE",
        }
    }
}

static mut SCRIPT_LOG_LEVEL: LogLevel = LogLevel::Info;

pub fn set_log_level(level: LogLevel) {
    unsafe { SCRIPT_LOG_LEVEL = level; }
}

pub fn get_log_level() -> LogLevel {
    unsafe { SCRIPT_LOG_LEVEL }
}

fn log(level: LogLevel, msg: &str) {
    let current_level = unsafe { SCRIPT_LOG_LEVEL };
    if (level as u8) <= (current_level as u8) {
        let color = match level {
            LogLevel::Error => "\x1b[1;31m",
            LogLevel::Warn => "\x1b[1;33m",
            LogLevel::Info => "\x1b[1;34m",
            LogLevel::Debug => "\x1b[0;36m",
            LogLevel::Trace => "\x1b[0;90m",
            LogLevel::Off => "",
        };
        crate::uart::write_str(color);
        crate::uart::write_str("[SCRIPT:");
        crate::uart::write_str(level.as_str());
        crate::uart::write_str("]\x1b[0m ");
        crate::uart::write_line(msg);
    }
}

macro_rules! log_error {
    ($($arg:tt)*) => { log(LogLevel::Error, &format!($($arg)*)); };
}

macro_rules! log_debug {
    ($($arg:tt)*) => { log(LogLevel::Debug, &format!($($arg)*)); };
}

macro_rules! log_trace {
    ($($arg:tt)*) => { log(LogLevel::Trace, &format!($($arg)*)); };
}

// ═══════════════════════════════════════════════════════════════════════════════
// OUTPUT BUFFER
// ═══════════════════════════════════════════════════════════════════════════════

static mut SCRIPT_OUTPUT: Option<Vec<u8>> = None;

fn init_output() {
    unsafe {
        SCRIPT_OUTPUT = Some(Vec::with_capacity(8192));
    }
}

fn take_output() -> Vec<u8> {
    unsafe {
        SCRIPT_OUTPUT.take().unwrap_or_default()
    }
}

fn append_output(s: &str) {
    unsafe {
        if let Some(ref mut buf) = SCRIPT_OUTPUT {
            buf.extend_from_slice(s.as_bytes());
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// ES6 IMPORT PREPROCESSOR
// ═══════════════════════════════════════════════════════════════════════════════

/// Preprocess script to handle ES6 import statements
/// Transforms:
///   import * as fs from "os:fs"     → let fs = __module_fs();
///   import { ls, read_file } from "os:fs"  → (stripped, functions are global)
fn preprocess_imports(script: &str) -> String {
    let mut output = String::with_capacity(script.len() + 256);
    
    for line in script.lines() {
        let trimmed = line.trim();
        
        // Skip empty lines and comments
        if trimmed.is_empty() || trimmed.starts_with("//") {
            output.push_str(line);
            output.push('\n');
            continue;
        }
        
        // Check for import statement
        if trimmed.starts_with("import ") && trimmed.contains(" from ") {
            // Extract the module name from quotes
            let module_name = extract_module_name(trimmed);
            
            if let Some(module) = module_name {
                // Check for: import * as NAME from "module"
                if trimmed.contains("* as ") {
                    if let Some(alias) = extract_namespace_alias(trimmed) {
                        // Transform to: let NAME = __module_X();
                        let fn_name = match module.as_str() {
                            "os:fs" => "__module_fs",
                            "os:net" => "__module_net",
                            "os:sys" => "__module_sys",
                            "os:mem" => "__module_mem",
                            _ => {
                                output.push_str(&format!("// Error: Unknown module '{}'\n", module));
                                continue;
                            }
                        };
                        output.push_str(&format!("let {} = {}();\n", alias, fn_name));
                        continue;
                    }
                }
                
                // Check for: import { fn1, fn2 } from "module" (named imports)
                // These are stripped since functions are global
                if trimmed.contains('{') && trimmed.contains('}') {
                    // Validate module exists
                    match module.as_str() {
                        "os:fs" | "os:net" | "os:sys" | "os:mem" => {
                            output.push_str(&format!("// imported from {}\n", module));
                            continue;
                        }
                        _ => {
                            output.push_str(&format!("// Error: Unknown module '{}'\n", module));
                            continue;
                        }
                    }
                }
                
                // Plain: import * from "module" (import all globals)
                if trimmed.contains("* from") && !trimmed.contains("* as") {
                    match module.as_str() {
                        "os:fs" | "os:net" | "os:sys" | "os:mem" => {
                            output.push_str(&format!("// imported {} globals\n", module));
                            continue;
                        }
                        _ => {
                            output.push_str(&format!("// Error: Unknown module '{}'\n", module));
                            continue;
                        }
                    }
                }
            }
        }
        
        output.push_str(line);
        output.push('\n');
    }
    
    output
}

/// Extract module name from import statement (between quotes)
fn extract_module_name(line: &str) -> Option<String> {
    // Find opening quote
    let start = line.find('"').or_else(|| line.find('\''))?;
    let rest = &line[start + 1..];
    // Find closing quote
    let end = rest.find('"').or_else(|| rest.find('\''))?;
    Some(rest[..end].to_string())
}

/// Extract namespace alias from "import * as NAME from ..."
fn extract_namespace_alias(line: &str) -> Option<String> {
    // Find "* as " pattern
    let as_pos = line.find("* as ")?;
    let after_as = &line[as_pos + 5..];
    // Find the alias (word before "from")
    let from_pos = after_as.find(" from")?;
    let alias = after_as[..from_pos].trim();
    if alias.is_empty() {
        None
    } else {
        Some(alias.to_string())
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// SCRIPT RUNTIME
// ═══════════════════════════════════════════════════════════════════════════════

pub struct ScriptRuntime {
    engine: Engine,
}

impl ScriptRuntime {
    // ═══════════════════════════════════════════════════════════════════════
    // os:fs MODULE - Filesystem functions
    // ═══════════════════════════════════════════════════════════════════════
    
    fn register_fs_module(engine: &mut Engine) {
        // ls() -> Array of {name, size, is_dir}
        engine.register_fn("ls", || -> Array {
            let mut list = Array::new();
            unsafe {
                if let (Some(fs), Some(dev)) = (crate::FS_STATE.as_ref(), crate::BLK_DEV.as_mut()) {
                    let files = fs.list_dir(dev, "/");
                    for f in files {
                        let mut map = Map::new();
                        map.insert("name".into(), Dynamic::from(f.name));
                        map.insert("size".into(), Dynamic::from(f.size as i64));
                        map.insert("is_dir".into(), Dynamic::from(f.is_dir));
                        list.push(Dynamic::from(map));
                    }
                }
            }
            list
        });
        
        // read_file(path) -> String
        engine.register_fn("read_file", |path: ImmutableString| -> ImmutableString {
            unsafe {
                if let (Some(fs), Some(dev)) = (crate::FS_STATE.as_ref(), crate::BLK_DEV.as_mut()) {
                    if let Some(data) = fs.read_file(dev, path.as_str()) {
                        return String::from_utf8_lossy(&data).into_owned().into();
                    }
                }
            }
            "".into()
        });
        
        // write_file(path, content) -> bool
        engine.register_fn("write_file", |path: ImmutableString, content: ImmutableString| -> bool {
            unsafe {
                if let (Some(fs), Some(dev)) = (crate::FS_STATE.as_mut(), crate::BLK_DEV.as_mut()) {
                    return fs.write_file(dev, path.as_str(), content.as_bytes()).is_ok();
                }
            }
            false
        });
        
        // file_exists(path) -> bool
        engine.register_fn("file_exists", |path: ImmutableString| -> bool {
            unsafe {
                if let (Some(fs), Some(dev)) = (crate::FS_STATE.as_ref(), crate::BLK_DEV.as_mut()) {
                    return fs.read_file(dev, path.as_str()).is_some();
                }
            }
            false
        });
        
        // fs_available() -> bool
        engine.register_fn("fs_available", || -> bool {
            unsafe { crate::FS_STATE.is_some() }
        });
    }
    
    // ═══════════════════════════════════════════════════════════════════════
    // os:net MODULE - Network functions
    // ═══════════════════════════════════════════════════════════════════════
    
    fn register_net_module(engine: &mut Engine) {
        // get_ip() -> String
        engine.register_fn("get_ip", || -> ImmutableString {
            let mut buf = [0u8; 16];
            let ip = crate::net::get_my_ip();
            let len = crate::net::format_ipv4(ip, &mut buf);
            String::from_utf8_lossy(&buf[..len]).into_owned().into()
        });
        
        // get_mac() -> String
        engine.register_fn("get_mac", || -> ImmutableString {
            unsafe {
                if let Some(ref state) = crate::NET_STATE {
                    return String::from_utf8_lossy(&state.mac_str()).into_owned().into();
                }
            }
            "00:00:00:00:00:00".into()
        });
        
        // get_gateway() -> String
        engine.register_fn("get_gateway", || -> ImmutableString {
            let mut buf = [0u8; 16];
            let len = crate::net::format_ipv4(crate::net::GATEWAY, &mut buf);
            String::from_utf8_lossy(&buf[..len]).into_owned().into()
        });
        
        // get_dns() -> String
        engine.register_fn("get_dns", || -> ImmutableString {
            let mut buf = [0u8; 16];
            let len = crate::net::format_ipv4(crate::net::DNS_SERVER, &mut buf);
            String::from_utf8_lossy(&buf[..len]).into_owned().into()
        });
        
        // get_prefix() -> i64
        engine.register_fn("get_prefix", || -> i64 {
            crate::net::PREFIX_LEN as i64
        });
        
        // net_available() -> bool
        engine.register_fn("net_available", || -> bool {
            unsafe { crate::NET_STATE.is_some() }
        });
    }
    
    // ═══════════════════════════════════════════════════════════════════════
    // os:sys MODULE - System functions
    // ═══════════════════════════════════════════════════════════════════════
    
    fn register_sys_module(engine: &mut Engine) {
        // time_ms() -> i64 (milliseconds since boot)
        engine.register_fn("time_ms", || -> i64 {
            const CLINT_MTIME: usize = 0x0200_BFF8;
            let mtime = unsafe { core::ptr::read_volatile(CLINT_MTIME as *const u64) };
            (mtime / 10_000) as i64
        });
        
        // sleep(ms)
        engine.register_fn("sleep", |ms: i64| {
            const CLINT_MTIME: usize = 0x0200_BFF8;
            let start = unsafe { core::ptr::read_volatile(CLINT_MTIME as *const u64) };
            let ticks = ms as u64 * 10_000;
            loop {
                let now = unsafe { core::ptr::read_volatile(CLINT_MTIME as *const u64) };
                if now.wrapping_sub(start) >= ticks { break; }
                core::hint::spin_loop();
                unsafe {
                    if let Some(ref mut net) = crate::NET_STATE {
                        net.poll((now / 10_000) as i64);
                    }
                }
            }
        });
        
        // cwd() -> String
        engine.register_fn("cwd", || -> ImmutableString {
            crate::cwd_get().into()
        });
        
        // kernel_version() -> String
        engine.register_fn("kernel_version", || -> ImmutableString {
            "RISK-V OS v0.1.0".into()
        });
        
        // arch() -> String
        engine.register_fn("arch", || -> ImmutableString {
            "RISC-V 64-bit (RV64GC)".into()
        });
    }
    
    // ═══════════════════════════════════════════════════════════════════════
    // os:mem MODULE - Memory functions
    // ═══════════════════════════════════════════════════════════════════════
    
    fn register_mem_module(engine: &mut Engine) {
        // heap_total() -> i64
        engine.register_fn("heap_total", || -> i64 {
            crate::allocator::heap_size() as i64
        });
        
        // heap_used() -> i64
        engine.register_fn("heap_used", || -> i64 {
            let (used, _) = crate::allocator::heap_stats();
            used as i64
        });
        
        // heap_free() -> i64
        engine.register_fn("heap_free", || -> i64 {
            let (_, free) = crate::allocator::heap_stats();
            free as i64
        });
        
        // heap_stats() -> {used, free}
        engine.register_fn("heap_stats", || -> Map {
            let (used, free) = crate::allocator::heap_stats();
            let mut map = Map::new();
            map.insert("used".into(), Dynamic::from(used as i64));
            map.insert("free".into(), Dynamic::from(free as i64));
            map
        });
    }
    
    // ═══════════════════════════════════════════════════════════════════════
    // MODULE OBJECTS - For namespace imports (import * as X from "...")
    // ═══════════════════════════════════════════════════════════════════════
    
    fn register_module_objects(engine: &mut Engine) {
        // Register module types
        engine.register_type_with_name::<FsModule>("FsModule");
        engine.register_type_with_name::<NetModule>("NetModule");
        engine.register_type_with_name::<SysModule>("SysModule");
        engine.register_type_with_name::<MemModule>("MemModule");
        
        // __module_fs() -> FsModule
        engine.register_fn("__module_fs", || FsModule);
        
        // FsModule methods
        engine.register_fn("ls", |_: &mut FsModule| -> Array {
            let mut list = Array::new();
            unsafe {
                if let (Some(fs), Some(dev)) = (crate::FS_STATE.as_ref(), crate::BLK_DEV.as_mut()) {
                    let files = fs.list_dir(dev, "/");
                    for f in files {
                        let mut map = Map::new();
                        map.insert("name".into(), Dynamic::from(f.name));
                        map.insert("size".into(), Dynamic::from(f.size as i64));
                        map.insert("is_dir".into(), Dynamic::from(f.is_dir));
                        list.push(Dynamic::from(map));
                    }
                }
            }
            list
        });
        engine.register_fn("read", |_: &mut FsModule, path: ImmutableString| -> ImmutableString {
            unsafe {
                if let (Some(fs), Some(dev)) = (crate::FS_STATE.as_ref(), crate::BLK_DEV.as_mut()) {
                    if let Some(data) = fs.read_file(dev, path.as_str()) {
                        return String::from_utf8_lossy(&data).into_owned().into();
                    }
                }
            }
            "".into()
        });
        engine.register_fn("write", |_: &mut FsModule, path: ImmutableString, content: ImmutableString| -> bool {
            unsafe {
                if let (Some(fs), Some(dev)) = (crate::FS_STATE.as_mut(), crate::BLK_DEV.as_mut()) {
                    return fs.write_file(dev, path.as_str(), content.as_bytes()).is_ok();
                }
            }
            false
        });
        engine.register_fn("exists", |_: &mut FsModule, path: ImmutableString| -> bool {
            unsafe {
                if let (Some(fs), Some(dev)) = (crate::FS_STATE.as_ref(), crate::BLK_DEV.as_mut()) {
                    return fs.read_file(dev, path.as_str()).is_some();
                }
            }
            false
        });
        engine.register_fn("available", |_: &mut FsModule| -> bool {
            unsafe { crate::FS_STATE.is_some() }
        });
        
        // __module_net() -> NetModule
        engine.register_fn("__module_net", || NetModule);
        
        // NetModule methods
        engine.register_fn("ip", |_: &mut NetModule| -> ImmutableString {
            let mut buf = [0u8; 16];
            let ip = crate::net::get_my_ip();
            let len = crate::net::format_ipv4(ip, &mut buf);
            String::from_utf8_lossy(&buf[..len]).into_owned().into()
        });
        engine.register_fn("mac", |_: &mut NetModule| -> ImmutableString {
            unsafe {
                if let Some(ref state) = crate::NET_STATE {
                    return String::from_utf8_lossy(&state.mac_str()).into_owned().into();
                }
            }
            "00:00:00:00:00:00".into()
        });
        engine.register_fn("gateway", |_: &mut NetModule| -> ImmutableString {
            let mut buf = [0u8; 16];
            let len = crate::net::format_ipv4(crate::net::GATEWAY, &mut buf);
            String::from_utf8_lossy(&buf[..len]).into_owned().into()
        });
        engine.register_fn("dns", |_: &mut NetModule| -> ImmutableString {
            let mut buf = [0u8; 16];
            let len = crate::net::format_ipv4(crate::net::DNS_SERVER, &mut buf);
            String::from_utf8_lossy(&buf[..len]).into_owned().into()
        });
        engine.register_fn("prefix", |_: &mut NetModule| -> i64 {
            crate::net::PREFIX_LEN as i64
        });
        engine.register_fn("available", |_: &mut NetModule| -> bool {
            unsafe { crate::NET_STATE.is_some() }
        });
        
        // __module_sys() -> SysModule
        engine.register_fn("__module_sys", || SysModule);
        
        // SysModule methods
        engine.register_fn("time", |_: &mut SysModule| -> i64 {
            const CLINT_MTIME: usize = 0x0200_BFF8;
            let mtime = unsafe { core::ptr::read_volatile(CLINT_MTIME as *const u64) };
            (mtime / 10_000) as i64
        });
        engine.register_fn("sleep", |_: &mut SysModule, ms: i64| {
            const CLINT_MTIME: usize = 0x0200_BFF8;
            let start = unsafe { core::ptr::read_volatile(CLINT_MTIME as *const u64) };
            let ticks = ms as u64 * 10_000;
            loop {
                let now = unsafe { core::ptr::read_volatile(CLINT_MTIME as *const u64) };
                if now.wrapping_sub(start) >= ticks { break; }
                core::hint::spin_loop();
                unsafe {
                    if let Some(ref mut net) = crate::NET_STATE {
                        net.poll((now / 10_000) as i64);
                    }
                }
            }
        });
        engine.register_fn("cwd", |_: &mut SysModule| -> ImmutableString {
            crate::cwd_get().into()
        });
        engine.register_fn("version", |_: &mut SysModule| -> ImmutableString {
            "RISK-V OS v0.1.0".into()
        });
        engine.register_fn("arch", |_: &mut SysModule| -> ImmutableString {
            "RISC-V 64-bit (RV64GC)".into()
        });
        
        // __module_mem() -> MemModule
        engine.register_fn("__module_mem", || MemModule);
        
        // MemModule methods
        engine.register_fn("total", |_: &mut MemModule| -> i64 {
            crate::allocator::heap_size() as i64
        });
        engine.register_fn("used", |_: &mut MemModule| -> i64 {
            let (used, _) = crate::allocator::heap_stats();
            used as i64
        });
        engine.register_fn("free", |_: &mut MemModule| -> i64 {
            let (_, free) = crate::allocator::heap_stats();
            free as i64
        });
        engine.register_fn("stats", |_: &mut MemModule| -> Map {
            let (used, free) = crate::allocator::heap_stats();
            let mut map = Map::new();
            map.insert("used".into(), Dynamic::from(used as i64));
            map.insert("free".into(), Dynamic::from(free as i64));
            map
        });
    }
    
    pub fn new() -> Self {
        log_debug!("Initializing JavaScript runtime...");
        
        let mut engine = Engine::new_raw();
        
        // Register StandardPackage
        let package = StandardPackage::new();
        engine.register_global_module(package.as_shared_module());
        
        // Engine limits
        engine.set_max_call_levels(64);
        engine.set_max_operations(1_000_000);
        engine.set_max_string_size(16384);
        engine.set_max_array_size(10000);
        engine.set_max_map_size(1000);
        engine.set_max_expr_depths(64, 64);
        
        // Register all module functions as globals
        Self::register_fs_module(&mut engine);
        Self::register_net_module(&mut engine);
        Self::register_sys_module(&mut engine);
        Self::register_mem_module(&mut engine);
        
        // Register module object constructors for namespace imports
        Self::register_module_objects(&mut engine);
        
        // ═══════════════════════════════════════════════════════════════════════
        // GLOBAL OUTPUT FUNCTIONS
        // ═══════════════════════════════════════════════════════════════════════
        
        engine.register_fn("print", |s: ImmutableString| {
            append_output(&s);
            append_output("\n");
        });
        engine.register_fn("print", |n: i64| {
            append_output(&format!("{}\n", n));
        });
        engine.register_fn("print", |n: f64| {
            append_output(&format!("{}\n", n));
        });
        engine.register_fn("print", |b: bool| {
            append_output(if b { "true\n" } else { "false\n" });
        });
        engine.register_fn("print", |arr: Array| {
            let s: Vec<String> = arr.iter().map(|v| v.to_string()).collect();
            append_output(&format!("[{}]\n", s.join(", ")));
        });
        engine.register_fn("print", |d: Dynamic| {
            append_output(&format!("{}\n", d));
        });
        
        engine.register_fn("write", |s: ImmutableString| {
            append_output(&s);
        });
        engine.register_fn("write", |n: i64| {
            append_output(&format!("{}", n));
        });
        
        engine.register_fn("debug", |d: Dynamic| {
            append_output(&format!("[DEBUG] {:?}\n", d));
        });
        
        // ═══════════════════════════════════════════════════════════════════════
        // GLOBAL UTILITY FUNCTIONS
        // ═══════════════════════════════════════════════════════════════════════
        
        engine.register_fn("parse_int", |s: ImmutableString| -> i64 {
            s.trim().parse::<i64>().unwrap_or(0)
        });
        
        engine.register_fn("parse_float", |s: ImmutableString| -> f64 {
            s.trim().parse::<f64>().unwrap_or(0.0)
        });
        
        engine.register_fn("type_of", |d: Dynamic| -> ImmutableString {
            d.type_name().into()
        });
        
        engine.register_fn("is_string", |d: Dynamic| -> bool {
            d.is::<ImmutableString>()
        });
        
        engine.register_fn("is_int", |d: Dynamic| -> bool {
            d.is::<i64>()
        });
        
        engine.register_fn("is_float", |d: Dynamic| -> bool {
            d.is::<f64>()
        });
        
        engine.register_fn("is_array", |d: Dynamic| -> bool {
            d.is::<Array>()
        });
        
        engine.register_fn("repeat", |s: ImmutableString, n: i64| -> ImmutableString {
            if n <= 0 { return "".into(); }
            let n = n.min(1000) as usize;
            s.repeat(n).into()
        });
        
        engine.register_fn("pad_left", |s: ImmutableString, width: i64, pad: ImmutableString| -> ImmutableString {
            let width = width.max(0) as usize;
            let pad_char = pad.chars().next().unwrap_or(' ');
            if s.len() >= width {
                s
            } else {
                let padding: String = core::iter::repeat(pad_char).take(width - s.len()).collect();
                format!("{}{}", padding, s).into()
            }
        });
        
        engine.register_fn("pad_right", |s: ImmutableString, width: i64, pad: ImmutableString| -> ImmutableString {
            let width = width.max(0) as usize;
            let pad_char = pad.chars().next().unwrap_or(' ');
            if s.len() >= width {
                s
            } else {
                let padding: String = core::iter::repeat(pad_char).take(width - s.len()).collect();
                format!("{}{}", s, padding).into()
            }
        });
        
        engine.register_fn("join", |arr: Array, sep: ImmutableString| -> ImmutableString {
            let strings: Vec<String> = arr.iter().map(|v| v.to_string()).collect();
            strings.join(sep.as_str()).into()
        });
        
        engine.register_fn("range", |end: i64| -> Array {
            (0..end.max(0).min(10000)).map(Dynamic::from).collect()
        });
        
        engine.register_fn("range", |start: i64, end: i64| -> Array {
            let start = start.min(10000);
            let end = end.min(10000);
            (start..end).map(Dynamic::from).collect()
        });
        
        engine.register_fn("range", |start: i64, end: i64, step: i64| -> Array {
            if step == 0 { return Array::new(); }
            let start = start.min(10000).max(-10000);
            let end = end.min(10000).max(-10000);
            let mut result = Array::new();
            let mut i = start;
            if step > 0 {
                while i < end && result.len() < 10000 {
                    result.push(Dynamic::from(i));
                    i += step;
                }
            } else {
                while i > end && result.len() < 10000 {
                    result.push(Dynamic::from(i));
                    i += step;
                }
            }
            result
        });
        
        log_debug!("JavaScript runtime initialized with module system");
        
        Self { engine }
    }
    
    pub fn execute(&self, script: &str, args: &[&str]) -> Result<String, String> {
        log_trace!("Executing script ({} bytes, {} args)", script.len(), args.len());
        
        // Preprocess ES6 imports
        let processed_script = preprocess_imports(script);
        log_trace!("Preprocessed script ({} bytes)", processed_script.len());
        
        init_output();
        
        let mut scope = Scope::new();
        let args_array: Array = args.iter()
            .map(|&s| Dynamic::from(ImmutableString::from(s)))
            .collect();
        scope.push("ARGS", args_array);
        
        match self.engine.eval_with_scope::<Dynamic>(&mut scope, &processed_script) {
            Ok(result) => {
                let output = take_output();
                log_trace!("Script completed successfully, output: {} bytes", output.len());
                
                if output.is_empty() && !result.is_unit() {
                    return Ok(format!("{}\n", result));
                }
                
                Ok(String::from_utf8_lossy(&output).into_owned())
            }
            Err(e) => {
                take_output();
                log_error!("Script execution failed: {}", e);
                Err(format!("{}", e))
            }
        }
    }
    
    pub fn compile(&self, script: &str) -> Result<(), String> {
        log_trace!("Compiling script ({} bytes)", script.len());
        match self.engine.compile(script) {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("Syntax error: {}", e))
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// PUBLIC API
// ═══════════════════════════════════════════════════════════════════════════════

pub fn execute_script(script_content: &str, args: &str) -> Result<String, String> {
    let args_vec: Vec<&str> = if args.is_empty() {
        Vec::new()
    } else {
        args.split_whitespace().collect()
    };
    let runtime = ScriptRuntime::new();
    runtime.execute(script_content, &args_vec)
}

pub fn find_script(cmd: &str) -> Option<Vec<u8>> {
    log_trace!("Looking for script: {}", cmd);
    
    unsafe {
        if let (Some(fs), Some(dev)) = (crate::FS_STATE.as_ref(), crate::BLK_DEV.as_mut()) {
            if cmd.contains('/') {
                let full_path = if cmd.starts_with('/') {
                    alloc::string::String::from(cmd)
                } else {
                    crate::resolve_path(cmd)
                };
                
                log_trace!("Resolved path: {} -> {}", cmd, full_path);
                
                if let Some(content) = fs.read_file(dev, &full_path) {
                    log_debug!("Found script at path: {} ({} bytes)", full_path, content.len());
                    return Some(content);
                }
                log_trace!("Script not found at path: {}", full_path);
                return None;
            }
            
            let usr_bin_path = format!("/usr/bin/{}", cmd);
            if let Some(content) = fs.read_file(dev, &usr_bin_path) {
                log_debug!("Found script in /usr/bin/: {} ({} bytes)", usr_bin_path, content.len());
                return Some(content);
            }
            
            if let Some(content) = fs.read_file(dev, cmd) {
                log_debug!("Found script in root: {} ({} bytes)", cmd, content.len());
                return Some(content);
            }
        }
    }
    
    log_trace!("Script not found: {}", cmd);
    None
}

pub fn print_info() {
    crate::uart::write_line("");
    crate::uart::write_line("\x1b[1;36m┌─────────────────────────────────────────────────────────────┐\x1b[0m");
    crate::uart::write_line("\x1b[1;36m│\x1b[0m              \x1b[1;97mJavaScript Runtime\x1b[0m                             \x1b[1;36m│\x1b[0m");
    crate::uart::write_line("\x1b[1;36m├─────────────────────────────────────────────────────────────┤\x1b[0m");
    crate::uart::write_line("\x1b[1;36m│\x1b[0m                                                             \x1b[1;36m│\x1b[0m");
    crate::uart::write_line("\x1b[1;36m│\x1b[0m  \x1b[1;33mImport Styles:\x1b[0m                                            \x1b[1;36m│\x1b[0m");
    crate::uart::write_line("\x1b[1;36m│\x1b[0m    import * as fs from \"os:fs\"     \x1b[0;90m// namespace import\x1b[0m    \x1b[1;36m│\x1b[0m");
    crate::uart::write_line("\x1b[1;36m│\x1b[0m    import { ls, read_file } from \"os:fs\"  \x1b[0;90m// named\x1b[0m        \x1b[1;36m│\x1b[0m");
    crate::uart::write_line("\x1b[1;36m│\x1b[0m                                                             \x1b[1;36m│\x1b[0m");
    crate::uart::write_line("\x1b[1;36m│\x1b[0m  \x1b[1;33mModules:\x1b[0m                                                  \x1b[1;36m│\x1b[0m");
    crate::uart::write_line("\x1b[1;36m│\x1b[0m    \x1b[1;32mos:fs\x1b[0m   ls() read(p) write(p,d) exists(p) available()   \x1b[1;36m│\x1b[0m");
    crate::uart::write_line("\x1b[1;36m│\x1b[0m    \x1b[1;32mos:net\x1b[0m  ip() mac() gateway() dns() prefix() available() \x1b[1;36m│\x1b[0m");
    crate::uart::write_line("\x1b[1;36m│\x1b[0m    \x1b[1;32mos:sys\x1b[0m  time() sleep(ms) cwd() version() arch()        \x1b[1;36m│\x1b[0m");
    crate::uart::write_line("\x1b[1;36m│\x1b[0m    \x1b[1;32mos:mem\x1b[0m  total() used() free() stats()                   \x1b[1;36m│\x1b[0m");
    crate::uart::write_line("\x1b[1;36m│\x1b[0m                                                             \x1b[1;36m│\x1b[0m");
    crate::uart::write_line("\x1b[1;36m│\x1b[0m  \x1b[1;33mGlobals:\x1b[0m  print() write() debug() ARGS                    \x1b[1;36m│\x1b[0m");
    crate::uart::write_line("\x1b[1;36m│\x1b[0m            parse_int() parse_float() join() range()...      \x1b[1;36m│\x1b[0m");
    crate::uart::write_line("\x1b[1;36m│\x1b[0m                                                             \x1b[1;36m│\x1b[0m");
    crate::uart::write_line("\x1b[1;36m│\x1b[0m  \x1b[1;33mLimits:\x1b[0m  call_depth=64  ops=1M  strings=16KB  arrays=10K  \x1b[1;36m│\x1b[0m");
    crate::uart::write_line("\x1b[1;36m└─────────────────────────────────────────────────────────────┘\x1b[0m");
    crate::uart::write_line("");
}
