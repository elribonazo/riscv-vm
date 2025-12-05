// kernel/src/scripting.rs
//! JavaScript-like scripting runtime with ES6 module system
//!
//! Scripts use `import * from` to import OS modules:
//!   import * from "os:fs"
//!   import * from "os:net"
//!   import * from "os:sys"
//!   import * from "os:mem"
//!
//! Performance optimizations:
//!   - Global cached runtime (created once, reused)
//!   - Compiled AST caching for frequently used scripts
//!   - Optimized import preprocessor

use crate::Spinlock;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use rhai::OptimizationLevel;
use rhai::{
    packages::{Package, StandardPackage},
    Array, Dynamic, Engine, ImmutableString, Map, Scope, AST,
};

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

/// HTTP module object - os:http
#[derive(Clone)]
pub struct HttpModule;

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

/// Script log level, protected by spinlock.
static SCRIPT_LOG_LEVEL: Spinlock<LogLevel> = Spinlock::new(LogLevel::Info);

pub fn set_log_level(level: LogLevel) {
    *SCRIPT_LOG_LEVEL.lock() = level;
}

pub fn get_log_level() -> LogLevel {
    *SCRIPT_LOG_LEVEL.lock()
}

fn log(level: LogLevel, msg: &str) {
    let current_level = *SCRIPT_LOG_LEVEL.lock();
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

/// Script output buffer, protected by spinlock.
static SCRIPT_OUTPUT: Spinlock<Option<Vec<u8>>> = Spinlock::new(None);

fn init_output() {
    *SCRIPT_OUTPUT.lock() = Some(Vec::with_capacity(8192));
}

fn take_output() -> Vec<u8> {
    SCRIPT_OUTPUT.lock().take().unwrap_or_default()
}

fn append_output(s: &str) {
    let mut guard = SCRIPT_OUTPUT.lock();
    if let Some(ref mut buf) = *guard {
        buf.extend_from_slice(s.as_bytes());
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// GLOBAL RUNTIME CACHE
// ═══════════════════════════════════════════════════════════════════════════════

// Note: ScriptRuntime contains Rhai Engine which uses Rc internally and is not Send.
// This is acceptable because scripts only run on the primary hart (shell).
// Access is serialized by the shell command loop.
static mut CACHED_RUNTIME: Option<ScriptRuntime> = None;

/// Get or create the global cached runtime (much faster than creating new each time)
fn get_runtime() -> &'static ScriptRuntime {
    unsafe {
        if CACHED_RUNTIME.is_none() {
            log_debug!("Creating cached script runtime...");
            CACHED_RUNTIME = Some(ScriptRuntime::new_internal());
        }
        CACHED_RUNTIME.as_ref().unwrap()
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// AST CACHE - Cache compiled scripts for faster re-execution
// ═══════════════════════════════════════════════════════════════════════════════

const AST_CACHE_MAX_SIZE: usize = 256;

// Note: AST contains Rhai types that use Rc internally and are not Send.
// This is acceptable because scripts only run on the primary hart (shell).
static mut AST_CACHE: Option<BTreeMap<u64, AST>> = None;

fn get_ast_cache() -> &'static mut BTreeMap<u64, AST> {
    unsafe {
        if AST_CACHE.is_none() {
            AST_CACHE = Some(BTreeMap::new());
        }
        AST_CACHE.as_mut().unwrap()
    }
}

/// Simple FNV-1a hash for script content (fast, good distribution)
#[inline]
fn hash_script(script: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    for byte in script.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Try to get cached AST, or compile and cache it
fn get_or_compile_ast(engine: &Engine, script: &str, hash: u64) -> Result<AST, String> {
    let cache = get_ast_cache();

    // Check cache first
    if let Some(ast) = cache.get(&hash) {
        log_trace!("AST cache hit for hash {:016x}", hash);
        return Ok(ast.clone());
    }

    // Compile new AST
    log_trace!("AST cache miss, compiling script...");
    let ast = engine
        .compile(script)
        .map_err(|e| format!("Syntax error: {}", e))?;

    // Evict oldest entries if cache is full (simple LRU approximation)
    if cache.len() >= AST_CACHE_MAX_SIZE {
        if let Some(&oldest_key) = cache.keys().next() {
            cache.remove(&oldest_key);
        }
    }

    // Cache the compiled AST
    cache.insert(hash, ast.clone());
    log_trace!("Cached AST, cache size: {}", cache.len());

    Ok(ast)
}

/// Clear the AST cache (useful for debugging or freeing memory)
pub fn clear_ast_cache() {
    unsafe {
        if let Some(ref mut cache) = AST_CACHE {
            cache.clear();
            log_debug!("AST cache cleared");
        }
    }
}

/// Preload all scripts from /usr/bin/ into the AST cache at boot
/// Returns the number of scripts successfully cached
pub fn preload_scripts() -> usize {
    log_debug!("Preloading scripts from /usr/bin/...");

    let runtime = get_runtime();
    let mut cached_count = 0;

    // Collect scripts with FS lock, then release before compiling
    let scripts_to_cache: Vec<(String, Vec<u8>)> = {
        let mut fs_guard = crate::FS_STATE.lock();
        let mut blk_guard = crate::BLK_DEV.lock();

        let mut scripts = Vec::new();
        if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
            // List all files in /usr/bin/
            let files = fs.list_dir(dev, "/usr/bin");

            for file_info in files {
                // Skip directories
                if file_info.is_dir {
                    continue;
                }

                let path = format!("/usr/bin/{}", file_info.name);

                // Read the script content
                if let Some(content) = fs.read_file(dev, &path) {
                    scripts.push((file_info.name.clone(), content));
                }
            }
        }
        scripts
    };

    // Now compile and cache scripts (FS lock released)
    for (name, content) in scripts_to_cache {
        if let Ok(script) = core::str::from_utf8(&content) {
            // Preprocess and cache
            let preprocess_result = preprocess_imports(script);
            let processed_script = preprocess_result.as_str(script);
            let script_hash = hash_script(processed_script);

            // Try to compile and cache
            match get_or_compile_ast(&runtime.engine, processed_script, script_hash) {
                Ok(_) => {
                    log_trace!("Cached: {}", name);
                    cached_count += 1;
                }
                Err(e) => {
                    log_error!("Failed to cache {}: {}", name, e);
                }
            }
        }
    }

    log_debug!("Preloaded {} scripts into AST cache", cached_count);
    cached_count
}

/// Get the current AST cache size
pub fn ast_cache_size() -> usize {
    unsafe { AST_CACHE.as_ref().map(|c| c.len()).unwrap_or(0) }
}

// ═══════════════════════════════════════════════════════════════════════════════
// ES6 IMPORT PREPROCESSOR
// ═══════════════════════════════════════════════════════════════════════════════

/// Preprocess script to handle ES6 import statements
/// Transforms:
///   import * as fs from "os:fs"     → let fs = __module_fs();
///   import { ls, read_file } from "os:fs"  → (stripped, functions are global)
///
/// Optimized: returns original script unchanged if no imports found (zero-copy)
fn preprocess_imports(script: &str) -> PreprocessResult {
    // Fast path: check if script contains any imports at all
    if !script.contains("import ") {
        return PreprocessResult::Unchanged;
    }

    let mut output = String::with_capacity(script.len() + 128);
    let mut had_imports = false;

    for line in script.lines() {
        let trimmed = line.trim();

        // Fast skip: empty lines, comments, or lines not starting with 'i'
        if trimmed.is_empty() || trimmed.starts_with("//") || !trimmed.starts_with("import ") {
            output.push_str(line);
            output.push('\n');
            continue;
        }

        // Must be an import line - check for " from "
        if !trimmed.contains(" from ") {
            output.push_str(line);
            output.push('\n');
            continue;
        }

        had_imports = true;

        // Extract module name (between quotes)
        let module = match extract_module_name_fast(trimmed) {
            Some(m) => m,
            None => {
                output.push_str(line);
                output.push('\n');
                continue;
            }
        };

        // Map module name to function name
        let module_fn = match module {
            "os:fs" => "__module_fs",
            "os:net" => "__module_net",
            "os:sys" => "__module_sys",
            "os:mem" => "__module_mem",
            "os:http" => "__module_http",
            _ => {
                output.push_str("// Error: Unknown module\n");
                continue;
            }
        };

        // Check for: import * as NAME from "module"
        if let Some(alias) = extract_namespace_alias_fast(trimmed) {
            output.push_str("let ");
            output.push_str(alias);
            output.push_str(" = ");
            output.push_str(module_fn);
            output.push_str("();\n");
            continue;
        }

        // Named imports or plain "import * from" - just strip them
        output.push_str("// imported\n");
    }

    if had_imports {
        PreprocessResult::Changed(output)
    } else {
        PreprocessResult::Unchanged
    }
}

/// Result of preprocessing - avoids allocation when no changes needed
enum PreprocessResult {
    Unchanged,
    Changed(String),
}

impl PreprocessResult {
    #[inline]
    fn as_str<'a>(&'a self, original: &'a str) -> &'a str {
        match self {
            PreprocessResult::Unchanged => original,
            PreprocessResult::Changed(s) => s.as_str(),
        }
    }
}

/// Extract module name from import statement (between quotes) - returns &str, no allocation
#[inline]
fn extract_module_name_fast(line: &str) -> Option<&str> {
    // Find " from " first, then look for quotes after it
    let from_pos = line.find(" from ")?;
    let after_from = &line[from_pos + 6..];

    // Find opening quote
    let start = after_from.find('"').or_else(|| after_from.find('\''))?;
    let rest = &after_from[start + 1..];
    // Find closing quote
    let end = rest.find('"').or_else(|| rest.find('\''))?;
    Some(&rest[..end])
}

/// Extract namespace alias from "import * as NAME from ..." - returns &str, no allocation
#[inline]
fn extract_namespace_alias_fast(line: &str) -> Option<&str> {
    // Find "* as " pattern
    let as_pos = line.find("* as ")?;
    let after_as = &line[as_pos + 5..];
    // Find the alias (word before "from")
    let from_pos = after_as.find(" from")?;
    let alias = after_as[..from_pos].trim();
    if alias.is_empty() {
        None
    } else {
        Some(alias)
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
            let mut fs_guard = crate::FS_STATE.lock();
            let mut blk_guard = crate::BLK_DEV.lock();
            if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
                let files = fs.list_dir(dev, "/");
                for f in files {
                    let mut map = Map::new();
                    map.insert("name".into(), Dynamic::from(f.name));
                    map.insert("size".into(), Dynamic::from(f.size as i64));
                    map.insert("is_dir".into(), Dynamic::from(f.is_dir));
                    list.push(Dynamic::from(map));
                }
            }
            list
        });

        // read_file(path) -> String
        engine.register_fn("read_file", |path: ImmutableString| -> ImmutableString {
            let fs_guard = crate::FS_STATE.lock();
            let mut blk_guard = crate::BLK_DEV.lock();
            if let (Some(fs), Some(dev)) = (fs_guard.as_ref(), blk_guard.as_mut()) {
                if let Some(data) = fs.read_file(dev, path.as_str()) {
                    return String::from_utf8_lossy(&data).into_owned().into();
                }
            }
            "".into()
        });

        // write_file(path, content) -> bool
        engine.register_fn(
            "write_file",
            |path: ImmutableString, content: ImmutableString| -> bool {
                let mut fs_guard = crate::FS_STATE.lock();
                let mut blk_guard = crate::BLK_DEV.lock();
                if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
                    return fs
                        .write_file(dev, path.as_str(), content.as_bytes())
                        .is_ok();
                }
                false
            },
        );

        // file_exists(path) -> bool
        engine.register_fn("file_exists", |path: ImmutableString| -> bool {
            let fs_guard = crate::FS_STATE.lock();
            let mut blk_guard = crate::BLK_DEV.lock();
            if let (Some(fs), Some(dev)) = (fs_guard.as_ref(), blk_guard.as_mut()) {
                return fs.read_file(dev, path.as_str()).is_some();
            }
            false
        });

        // fs_available() -> bool
        engine.register_fn("fs_available", || -> bool {
            crate::FS_STATE.lock().is_some()
        });

        // mkdir(path) -> bool
        engine.register_fn("mkdir", |path: ImmutableString| -> bool {
            let mut fs_guard = crate::FS_STATE.lock();
            let mut blk_guard = crate::BLK_DEV.lock();
            if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
                return fs.mkdir(dev, path.as_str()).is_ok();
            }
            false
        });

        // rmdir(path) -> bool
        engine.register_fn("rmdir", |path: ImmutableString| -> bool {
            let mut fs_guard = crate::FS_STATE.lock();
            let mut blk_guard = crate::BLK_DEV.lock();
            if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
                let dir_path = if path.ends_with('/') {
                    path.to_string()
                } else {
                    format!("{}/", path)
                };
                return fs.remove(dev, &dir_path).is_ok();
            }
            false
        });

        // rm(path) -> bool
        engine.register_fn("rm", |path: ImmutableString| -> bool {
            let mut fs_guard = crate::FS_STATE.lock();
            let mut blk_guard = crate::BLK_DEV.lock();
            if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
                return fs.remove(dev, path.as_str()).is_ok();
            }
            false
        });

        // is_dir(path) -> bool
        engine.register_fn("is_dir", |path: ImmutableString| -> bool {
            let mut fs_guard = crate::FS_STATE.lock();
            let mut blk_guard = crate::BLK_DEV.lock();
            if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
                return fs.is_dir(dev, path.as_str());
            }
            false
        });

        // fs_sync() -> i64 (number of blocks synced)
        engine.register_fn("fs_sync", || -> i64 {
            let mut fs_guard = crate::FS_STATE.lock();
            let mut blk_guard = crate::BLK_DEV.lock();
            if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
                return fs.sync(dev).unwrap_or(0) as i64;
            }
            0
        });

        // fs_cache_stats() -> {hits, misses, writebacks, cached}
        engine.register_fn("fs_cache_stats", || -> Map {
            let fs_guard = crate::FS_STATE.lock();
            let mut map = Map::new();
            if let Some(fs) = fs_guard.as_ref() {
                let (hits, misses, writebacks, cached) = fs.cache_stats();
                map.insert("hits".into(), Dynamic::from(hits as i64));
                map.insert("misses".into(), Dynamic::from(misses as i64));
                map.insert("writebacks".into(), Dynamic::from(writebacks as i64));
                map.insert("cached".into(), Dynamic::from(cached as i64));
            }
            map
        });

        // fs_dirty_blocks() -> i64
        engine.register_fn("fs_dirty_blocks", || -> i64 {
            let fs_guard = crate::FS_STATE.lock();
            if let Some(fs) = fs_guard.as_ref() {
                return fs.dirty_blocks() as i64;
            }
            0
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
            let net_guard = crate::NET_STATE.lock();
            if let Some(ref state) = *net_guard {
                return String::from_utf8_lossy(&state.mac_str())
                    .into_owned()
                    .into();
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
        engine.register_fn("get_prefix", || -> i64 { crate::net::PREFIX_LEN as i64 });

        // net_available() -> bool
        engine.register_fn("net_available", || -> bool {
            crate::NET_STATE.lock().is_some()
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
                if now.wrapping_sub(start) >= ticks {
                    break;
                }
                core::hint::spin_loop();
                {
                    let mut net_guard = crate::NET_STATE.lock();
                    if let Some(ref mut net) = *net_guard {
                        net.poll((now / 10_000) as i64);
                    }
                }
            }
        });

        // cwd() -> String
        engine.register_fn("cwd", || -> ImmutableString { crate::cwd_get().into() });

        // kernel_version() -> String
        engine.register_fn("kernel_version", || -> ImmutableString {
            const VERSION: &str = env!("CARGO_PKG_VERSION");
            format!("BAVY OS v{}", VERSION).into()
        });

        // arch() -> String
        engine.register_fn("arch", || -> ImmutableString {
            "RISC-V 64-bit (RV64GC)".into()
        });

        // harts_online() -> i64
        engine.register_fn("harts_online", || -> i64 {
            crate::HARTS_ONLINE.load(core::sync::atomic::Ordering::Relaxed) as i64
        });
    }

    // ═══════════════════════════════════════════════════════════════════════
    // os:proc MODULE - Process management functions
    // ═══════════════════════════════════════════════════════════════════════

    fn register_proc_module(engine: &mut Engine) {
        // get_tasks() -> Array of {pid, name, state, priority, hart, cpu_time, uptime}
        engine.register_fn("get_tasks", || -> Array {
            let mut list = Array::new();
            let tasks = crate::scheduler::SCHEDULER.list_tasks();
            for task in tasks {
                let mut map = Map::new();
                map.insert("pid".into(), Dynamic::from(task.pid as i64));
                map.insert("name".into(), Dynamic::from(task.name));
                map.insert("state".into(), Dynamic::from(task.state.as_str()));
                map.insert("priority".into(), Dynamic::from(task.priority.as_str()));
                map.insert(
                    "hart".into(),
                    Dynamic::from(task.hart.map(|h| h as i64).unwrap_or(-1)),
                );
                map.insert("cpu_time".into(), Dynamic::from(task.cpu_time as i64));
                map.insert("uptime".into(), Dynamic::from(task.uptime as i64));
                list.push(Dynamic::from(map));
            }
            list
        });

        // task_count() -> i64
        engine.register_fn("task_count", || -> i64 {
            crate::scheduler::SCHEDULER.task_count() as i64
        });

        // kill_task(pid) -> bool
        engine.register_fn("kill_task", |pid: i64| -> bool {
            if pid <= 0 {
                return false;
            }
            crate::scheduler::SCHEDULER.kill(pid as u32)
        });

        // get_klog(count) -> Array of formatted log strings
        engine.register_fn("get_klog", |count: i64| -> Array {
            let count = count.max(1).min(100) as usize;
            let entries = crate::klog::KLOG.recent(count);
            entries
                .iter()
                .rev() // Most recent first
                .map(|e| Dynamic::from(e.format_colored()))
                .collect()
        });

        // services() -> Array of {name, pid, status, started_at, hart}
        engine.register_fn("services", || -> Array {
            let mut list = Array::new();
            let services = crate::init::list_services();
            for svc in services {
                let mut map = Map::new();
                map.insert("name".into(), Dynamic::from(svc.name));
                map.insert("pid".into(), Dynamic::from(svc.pid as i64));
                map.insert("status".into(), Dynamic::from(svc.status.as_str()));
                map.insert("started_at".into(), Dynamic::from(svc.started_at as i64));
                map.insert(
                    "hart".into(),
                    Dynamic::from(svc.hart.map(|h| h as i64).unwrap_or(-1)),
                );
                list.push(Dynamic::from(map));
            }
            list
        });

        // service_defs() -> Array of {name, description}
        engine.register_fn("service_defs", || -> Array {
            let mut list = Array::new();
            let defs = crate::init::list_service_defs();
            for (name, desc) in defs {
                let mut map = Map::new();
                map.insert("name".into(), Dynamic::from(name));
                map.insert("description".into(), Dynamic::from(desc));
                list.push(Dynamic::from(map));
            }
            list
        });

        // start_service(name) -> bool
        engine.register_fn("start_service", |name: ImmutableString| -> bool {
            match crate::init::start_service(name.as_str()) {
                Ok(()) => true,
                Err(_) => false,
            }
        });

        // stop_service(name) -> bool
        engine.register_fn("stop_service", |name: ImmutableString| -> bool {
            match crate::init::stop_service(name.as_str()) {
                Ok(()) => true,
                Err(_) => false,
            }
        });

        // restart_service(name) -> bool
        engine.register_fn("restart_service", |name: ImmutableString| -> bool {
            match crate::init::restart_service(name.as_str()) {
                Ok(()) => true,
                Err(_) => false,
            }
        });

        // service_status(name) -> String ("running", "stopped", "failed", or "unknown")
        engine.register_fn(
            "service_status",
            |name: ImmutableString| -> ImmutableString {
                match crate::init::service_status(name.as_str()) {
                    Some(status) => status.as_str().into(),
                    None => "unknown".into(),
                }
            },
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    // os:ipc MODULE - Inter-Process Communication functions
    // ═══════════════════════════════════════════════════════════════════════

    fn register_ipc_module(engine: &mut Engine) {
        // create_channel(name) -> channel_id
        engine.register_fn("create_channel", |name: ImmutableString| -> i64 {
            let channel = crate::ipc::IPC.create_channel(name.as_str());
            channel.id as i64
        });

        // get_channel(name) -> channel_id or -1
        engine.register_fn("get_channel", |name: ImmutableString| -> i64 {
            match crate::ipc::IPC.get_channel_by_name(name.as_str()) {
                Some(ch) => ch.id as i64,
                None => -1,
            }
        });

        // channel_send(channel_id, message, msg_type) -> bool
        engine.register_fn(
            "channel_send",
            |channel_id: i64, message: ImmutableString, msg_type: i64| -> bool {
                if channel_id < 0 {
                    return false;
                }
                if let Some(channel) = crate::ipc::IPC.get_channel(channel_id as u32) {
                    let msg = crate::ipc::Message::from_str(0, message.as_str(), msg_type as u32);
                    channel.send(msg).is_ok()
                } else {
                    false
                }
            },
        );

        // channel_recv(channel_id) -> {ok, sender, data, msg_type} or {ok: false}
        engine.register_fn("channel_recv", |channel_id: i64| -> Map {
            let mut result = Map::new();
            if channel_id < 0 {
                result.insert("ok".into(), Dynamic::from(false));
                return result;
            }
            if let Some(channel) = crate::ipc::IPC.get_channel(channel_id as u32) {
                if let Some(msg) = channel.try_recv() {
                    result.insert("ok".into(), Dynamic::from(true));
                    result.insert("sender".into(), Dynamic::from(msg.sender as i64));
                    result.insert("data".into(), Dynamic::from(msg.as_str()));
                    result.insert("msg_type".into(), Dynamic::from(msg.msg_type as i64));
                } else {
                    result.insert("ok".into(), Dynamic::from(false));
                }
            } else {
                result.insert("ok".into(), Dynamic::from(false));
            }
            result
        });

        // channel_has_messages(channel_id) -> bool
        engine.register_fn("channel_has_messages", |channel_id: i64| -> bool {
            if channel_id < 0 {
                return false;
            }
            crate::ipc::IPC
                .get_channel(channel_id as u32)
                .map(|ch| ch.has_messages())
                .unwrap_or(false)
        });

        // channel_close(channel_id)
        engine.register_fn("channel_close", |channel_id: i64| {
            if channel_id >= 0 {
                crate::ipc::IPC.remove_channel(channel_id as u32);
            }
        });

        // create_pipe() -> pipe_id
        engine.register_fn("create_pipe", || -> i64 {
            let pipe = crate::ipc::IPC.create_pipe();
            pipe.id as i64
        });

        // pipe_write(pipe_id, data) -> bytes_written or -1 on error
        engine.register_fn("pipe_write", |pipe_id: i64, data: ImmutableString| -> i64 {
            if pipe_id < 0 {
                return -1;
            }
            if let Some(pipe) = crate::ipc::IPC.get_pipe(pipe_id as u32) {
                match pipe.write(data.as_bytes()) {
                    Ok(n) => n as i64,
                    Err(_) => -1,
                }
            } else {
                -1
            }
        });

        // pipe_read(pipe_id, max_bytes) -> string or empty on error/no data
        engine.register_fn(
            "pipe_read",
            |pipe_id: i64, max_bytes: i64| -> ImmutableString {
                if pipe_id < 0 || max_bytes <= 0 {
                    return "".into();
                }
                if let Some(pipe) = crate::ipc::IPC.get_pipe(pipe_id as u32) {
                    let mut buf = alloc::vec![0u8; max_bytes.min(8192) as usize];
                    match pipe.read(&mut buf) {
                        Ok(n) => String::from_utf8_lossy(&buf[..n]).into_owned().into(),
                        Err(_) => "".into(),
                    }
                } else {
                    "".into()
                }
            },
        );

        // pipe_available(pipe_id) -> bytes available to read
        engine.register_fn("pipe_available", |pipe_id: i64| -> i64 {
            if pipe_id < 0 {
                return 0;
            }
            crate::ipc::IPC
                .get_pipe(pipe_id as u32)
                .map(|p| p.available() as i64)
                .unwrap_or(0)
        });

        // pipe_close(pipe_id)
        engine.register_fn("pipe_close", |pipe_id: i64| {
            if pipe_id >= 0 {
                crate::ipc::IPC.remove_pipe(pipe_id as u32);
            }
        });

        // list_channels() -> Array of {id, name, pending}
        engine.register_fn("list_channels", || -> Array {
            crate::ipc::IPC
                .list_channels()
                .into_iter()
                .map(|(id, name, pending)| {
                    let mut map = Map::new();
                    map.insert("id".into(), Dynamic::from(id as i64));
                    map.insert("name".into(), Dynamic::from(name));
                    map.insert("pending".into(), Dynamic::from(pending as i64));
                    Dynamic::from(map)
                })
                .collect()
        });

        // list_pipes() -> Array of {id, available, closed}
        engine.register_fn("list_pipes", || -> Array {
            crate::ipc::IPC
                .list_pipes()
                .into_iter()
                .map(|(id, available, closed)| {
                    let mut map = Map::new();
                    map.insert("id".into(), Dynamic::from(id as i64));
                    map.insert("available".into(), Dynamic::from(available as i64));
                    map.insert("closed".into(), Dynamic::from(closed));
                    Dynamic::from(map)
                })
                .collect()
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
    // os:http MODULE - HTTP client functions
    // ═══════════════════════════════════════════════════════════════════════

    fn register_http_module(engine: &mut Engine) {
        /// Helper to get time in milliseconds
        fn get_time_ms() -> i64 {
            const CLINT_MTIME: usize = 0x0200_BFF8;
            let mtime = unsafe { core::ptr::read_volatile(CLINT_MTIME as *const u64) };
            (mtime / 10_000) as i64
        }

        // http_request(options) -> {ok, status, statusText, headers, body}
        // options = {url, method?, headers?, body?, timeout?}
        engine.register_fn("http_request", |options: Map| -> Map {
            let mut result = Map::new();

            // Extract URL (required)
            let url = match options.get("url") {
                Some(v) => v.clone().into_string().unwrap_or_default(),
                None => {
                    result.insert("ok".into(), Dynamic::from(false));
                    result.insert("error".into(), Dynamic::from("Missing 'url' in options"));
                    return result;
                }
            };

            // Extract method (default: GET)
            let method_str = options
                .get("method")
                .map(|v| v.clone().into_string().unwrap_or_default())
                .unwrap_or_else(|| "GET".to_string());

            let method = match method_str.to_uppercase().as_str() {
                "GET" => crate::http::HttpMethod::Get,
                "POST" => crate::http::HttpMethod::Post,
                "PUT" => crate::http::HttpMethod::Put,
                "DELETE" => crate::http::HttpMethod::Delete,
                "HEAD" => crate::http::HttpMethod::Head,
                _ => {
                    result.insert("ok".into(), Dynamic::from(false));
                    result.insert("error".into(), Dynamic::from("Invalid HTTP method"));
                    return result;
                }
            };

            // Extract timeout (default: 10000ms)
            let timeout = options
                .get("timeout")
                .and_then(|v| v.clone().try_cast::<i64>())
                .unwrap_or(10000);

            // Extract followRedirects option (default: true)
            let follow_redirects = options
                .get("followRedirects")
                .and_then(|v| v.clone().try_cast::<bool>())
                .unwrap_or(true);

            // Build the request
            let mut request = match crate::http::HttpRequest::new(method, &url) {
                Ok(r) => r,
                Err(e) => {
                    result.insert("ok".into(), Dynamic::from(false));
                    result.insert("error".into(), Dynamic::from(e));
                    return result;
                }
            };

            // Extract custom headers
            if let Some(headers_val) = options.get("headers") {
                if let Some(headers_map) = headers_val.clone().try_cast::<Map>() {
                    for (key, value) in headers_map.iter() {
                        if let Ok(v) = value.clone().into_string() {
                            request.headers.insert(key.to_string(), v);
                        }
                    }
                }
            }

            // Extract body
            if let Some(body_val) = options.get("body") {
                if let Ok(body_str) = body_val.clone().into_string() {
                    request = request.body_str(&body_str);
                }
            }

            // Perform the request
            {
                let mut net_guard = crate::NET_STATE.lock();
                if let Some(ref mut net) = *net_guard {
                    let http_result = if follow_redirects {
                        crate::http::http_request_follow_redirects(
                            net,
                            &request,
                            timeout,
                            get_time_ms,
                        )
                    } else {
                        crate::http::http_request(net, &request, timeout, get_time_ms)
                    };
                    match http_result {
                        Ok(response) => {
                            // Extract body first (needs borrow), then move other fields
                            let body_text = response.text();
                            let status_code = response.status_code;
                            let status_text = response.status_text;

                            result.insert("ok".into(), Dynamic::from(true));
                            result.insert("status".into(), Dynamic::from(status_code as i64));
                            result.insert("statusText".into(), Dynamic::from(status_text));

                            // Convert headers to Map
                            let mut headers_map = Map::new();
                            for (key, value) in response.headers {
                                headers_map.insert(key.into(), Dynamic::from(value));
                            }
                            result.insert("headers".into(), Dynamic::from(headers_map));
                            result.insert("body".into(), Dynamic::from(body_text));
                        }
                        Err(e) => {
                            result.insert("ok".into(), Dynamic::from(false));
                            result.insert("error".into(), Dynamic::from(e));
                        }
                    }
                } else {
                    result.insert("ok".into(), Dynamic::from(false));
                    result.insert("error".into(), Dynamic::from("Network not available"));
                }
            }

            result
        });

        // http_get(url) -> {ok, status, body, ...}
        // Automatically follows redirects
        engine.register_fn("http_get", |url: ImmutableString| -> Map {
            let mut result = Map::new();

            let mut net_guard = crate::NET_STATE.lock();
            if let Some(ref mut net) = *net_guard {
                match crate::http::get_follow_redirects(net, url.as_str(), 10000, get_time_ms) {
                    Ok(response) => {
                        let body_text = response.text();
                        let status_code = response.status_code;
                        let status_text = response.status_text;

                        result.insert("ok".into(), Dynamic::from(true));
                        result.insert("status".into(), Dynamic::from(status_code as i64));
                        result.insert("statusText".into(), Dynamic::from(status_text));

                        let mut headers_map = Map::new();
                        for (key, value) in response.headers {
                            headers_map.insert(key.into(), Dynamic::from(value));
                        }
                        result.insert("headers".into(), Dynamic::from(headers_map));
                        result.insert("body".into(), Dynamic::from(body_text));
                    }
                    Err(e) => {
                        result.insert("ok".into(), Dynamic::from(false));
                        result.insert("error".into(), Dynamic::from(e));
                    }
                }
            } else {
                result.insert("ok".into(), Dynamic::from(false));
                result.insert("error".into(), Dynamic::from("Network not available"));
            }

            result
        });

        // http_post(url, body, content_type) -> {ok, status, body, ...}
        engine.register_fn(
            "http_post",
            |url: ImmutableString, body: ImmutableString, content_type: ImmutableString| -> Map {
                let mut result = Map::new();

                let mut net_guard = crate::NET_STATE.lock();
                if let Some(ref mut net) = *net_guard {
                    match crate::http::post(
                        net,
                        url.as_str(),
                        body.as_str(),
                        content_type.as_str(),
                        10000,
                        get_time_ms,
                    ) {
                        Ok(response) => {
                            let body_text = response.text();
                            let status_code = response.status_code;
                            let status_text = response.status_text;

                            result.insert("ok".into(), Dynamic::from(true));
                            result.insert("status".into(), Dynamic::from(status_code as i64));
                            result.insert("statusText".into(), Dynamic::from(status_text));

                            let mut headers_map = Map::new();
                            for (key, value) in response.headers {
                                headers_map.insert(key.into(), Dynamic::from(value));
                            }
                            result.insert("headers".into(), Dynamic::from(headers_map));
                            result.insert("body".into(), Dynamic::from(body_text));
                        }
                        Err(e) => {
                            result.insert("ok".into(), Dynamic::from(false));
                            result.insert("error".into(), Dynamic::from(e));
                        }
                    }
                } else {
                    result.insert("ok".into(), Dynamic::from(false));
                    result.insert("error".into(), Dynamic::from("Network not available"));
                }

                result
            },
        );
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
        engine.register_type_with_name::<HttpModule>("HttpModule");

        // __module_fs() -> FsModule
        engine.register_fn("__module_fs", || FsModule);

        // FsModule methods
        engine.register_fn("ls", |_: &mut FsModule| -> Array {
            let mut list = Array::new();
            let mut fs_guard = crate::FS_STATE.lock();
            let mut blk_guard = crate::BLK_DEV.lock();
            if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
                let files = fs.list_dir(dev, "/");
                for f in files {
                    let mut map = Map::new();
                    map.insert("name".into(), Dynamic::from(f.name));
                    map.insert("size".into(), Dynamic::from(f.size as i64));
                    map.insert("is_dir".into(), Dynamic::from(f.is_dir));
                    list.push(Dynamic::from(map));
                }
            }
            list
        });
        engine.register_fn(
            "read",
            |_: &mut FsModule, path: ImmutableString| -> ImmutableString {
                let fs_guard = crate::FS_STATE.lock();
                let mut blk_guard = crate::BLK_DEV.lock();
                if let (Some(fs), Some(dev)) = (fs_guard.as_ref(), blk_guard.as_mut()) {
                    if let Some(data) = fs.read_file(dev, path.as_str()) {
                        return String::from_utf8_lossy(&data).into_owned().into();
                    }
                }
                "".into()
            },
        );
        engine.register_fn(
            "write",
            |_: &mut FsModule, path: ImmutableString, content: ImmutableString| -> bool {
                let mut fs_guard = crate::FS_STATE.lock();
                let mut blk_guard = crate::BLK_DEV.lock();
                if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
                    return fs
                        .write_file(dev, path.as_str(), content.as_bytes())
                        .is_ok();
                }
                false
            },
        );
        engine.register_fn(
            "exists",
            |_: &mut FsModule, path: ImmutableString| -> bool {
                let fs_guard = crate::FS_STATE.lock();
                let mut blk_guard = crate::BLK_DEV.lock();
                if let (Some(fs), Some(dev)) = (fs_guard.as_ref(), blk_guard.as_mut()) {
                    return fs.read_file(dev, path.as_str()).is_some();
                }
                false
            },
        );
        engine.register_fn("available", |_: &mut FsModule| -> bool {
            crate::FS_STATE.lock().is_some()
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
            let net_guard = crate::NET_STATE.lock();
            if let Some(ref state) = *net_guard {
                return String::from_utf8_lossy(&state.mac_str())
                    .into_owned()
                    .into();
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
            crate::NET_STATE.lock().is_some()
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
                if now.wrapping_sub(start) >= ticks {
                    break;
                }
                core::hint::spin_loop();
                {
                    let mut net_guard = crate::NET_STATE.lock();
                    if let Some(ref mut net) = *net_guard {
                        net.poll((now / 10_000) as i64);
                    }
                }
            }
        });
        engine.register_fn("cwd", |_: &mut SysModule| -> ImmutableString {
            crate::cwd_get().into()
        });
        engine.register_fn("version", |_: &mut SysModule| -> ImmutableString {
            const VERSION: &str = env!("CARGO_PKG_VERSION");
            format!("BAVY OS v{}", VERSION).into()
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

        // __module_http() -> HttpModule
        engine.register_fn("__module_http", || HttpModule);

        /// Helper to get time in milliseconds
        fn get_time_ms_mod() -> i64 {
            const CLINT_MTIME: usize = 0x0200_BFF8;
            let mtime = unsafe { core::ptr::read_volatile(CLINT_MTIME as *const u64) };
            (mtime / 10_000) as i64
        }

        // HttpModule methods
        // http.get(url) -> response object
        // Automatically follows redirects
        engine.register_fn("get", |_: &mut HttpModule, url: ImmutableString| -> Map {
            let mut result = Map::new();

            let mut net_guard = crate::NET_STATE.lock();
            if let Some(ref mut net) = *net_guard {
                match crate::http::get_follow_redirects(net, url.as_str(), 10000, get_time_ms_mod) {
                    Ok(response) => {
                        let body_text = response.text();
                        let status_code = response.status_code;
                        let status_text = response.status_text;

                        result.insert("ok".into(), Dynamic::from(true));
                        result.insert("status".into(), Dynamic::from(status_code as i64));
                        result.insert("statusText".into(), Dynamic::from(status_text));

                        let mut headers_map = Map::new();
                        for (key, value) in response.headers {
                            headers_map.insert(key.into(), Dynamic::from(value));
                        }
                        result.insert("headers".into(), Dynamic::from(headers_map));
                        result.insert("body".into(), Dynamic::from(body_text));
                    }
                    Err(e) => {
                        result.insert("ok".into(), Dynamic::from(false));
                        result.insert("error".into(), Dynamic::from(e));
                    }
                }
            } else {
                result.insert("ok".into(), Dynamic::from(false));
                result.insert("error".into(), Dynamic::from("Network not available"));
            }

            result
        });

        // http.post(url, body, content_type) -> response object
        engine.register_fn(
            "post",
            |_: &mut HttpModule,
             url: ImmutableString,
             body: ImmutableString,
             content_type: ImmutableString|
             -> Map {
                let mut result = Map::new();

                let mut net_guard = crate::NET_STATE.lock();
                if let Some(ref mut net) = *net_guard {
                    match crate::http::post(
                        net,
                        url.as_str(),
                        body.as_str(),
                        content_type.as_str(),
                        10000,
                        get_time_ms_mod,
                    ) {
                        Ok(response) => {
                            let body_text = response.text();
                            let status_code = response.status_code;
                            let status_text = response.status_text;

                            result.insert("ok".into(), Dynamic::from(true));
                            result.insert("status".into(), Dynamic::from(status_code as i64));
                            result.insert("statusText".into(), Dynamic::from(status_text));

                            let mut headers_map = Map::new();
                            for (key, value) in response.headers {
                                headers_map.insert(key.into(), Dynamic::from(value));
                            }
                            result.insert("headers".into(), Dynamic::from(headers_map));
                            result.insert("body".into(), Dynamic::from(body_text));
                        }
                        Err(e) => {
                            result.insert("ok".into(), Dynamic::from(false));
                            result.insert("error".into(), Dynamic::from(e));
                        }
                    }
                } else {
                    result.insert("ok".into(), Dynamic::from(false));
                    result.insert("error".into(), Dynamic::from("Network not available"));
                }

                result
            },
        );

        // http.request(options) -> response object
        engine.register_fn("request", |_: &mut HttpModule, options: Map| -> Map {
            let mut result = Map::new();

            // Extract URL (required)
            let url = match options.get("url") {
                Some(v) => v.clone().into_string().unwrap_or_default(),
                None => {
                    result.insert("ok".into(), Dynamic::from(false));
                    result.insert("error".into(), Dynamic::from("Missing 'url' in options"));
                    return result;
                }
            };

            // Extract method (default: GET)
            let method_str = options
                .get("method")
                .map(|v| v.clone().into_string().unwrap_or_default())
                .unwrap_or_else(|| "GET".to_string());

            let method = match method_str.to_uppercase().as_str() {
                "GET" => crate::http::HttpMethod::Get,
                "POST" => crate::http::HttpMethod::Post,
                "PUT" => crate::http::HttpMethod::Put,
                "DELETE" => crate::http::HttpMethod::Delete,
                "HEAD" => crate::http::HttpMethod::Head,
                _ => {
                    result.insert("ok".into(), Dynamic::from(false));
                    result.insert("error".into(), Dynamic::from("Invalid HTTP method"));
                    return result;
                }
            };

            // Extract timeout (default: 10000ms)
            let timeout = options
                .get("timeout")
                .and_then(|v| v.clone().try_cast::<i64>())
                .unwrap_or(10000);

            // Build the request
            let mut request = match crate::http::HttpRequest::new(method, &url) {
                Ok(r) => r,
                Err(e) => {
                    result.insert("ok".into(), Dynamic::from(false));
                    result.insert("error".into(), Dynamic::from(e));
                    return result;
                }
            };

            // Extract custom headers
            if let Some(headers_val) = options.get("headers") {
                if let Some(headers_map) = headers_val.clone().try_cast::<Map>() {
                    for (key, value) in headers_map.iter() {
                        if let Ok(v) = value.clone().into_string() {
                            request.headers.insert(key.to_string(), v);
                        }
                    }
                }
            }

            // Extract body
            if let Some(body_val) = options.get("body") {
                if let Ok(body_str) = body_val.clone().into_string() {
                    request = request.body_str(&body_str);
                }
            }

            // Perform the request
            {
                let mut net_guard = crate::NET_STATE.lock();
                if let Some(ref mut net) = *net_guard {
                    match crate::http::http_request(net, &request, timeout, get_time_ms_mod) {
                        Ok(response) => {
                            let body_text = response.text();
                            let status_code = response.status_code;
                            let status_text = response.status_text;

                            result.insert("ok".into(), Dynamic::from(true));
                            result.insert("status".into(), Dynamic::from(status_code as i64));
                            result.insert("statusText".into(), Dynamic::from(status_text));

                            let mut headers_map = Map::new();
                            for (key, value) in response.headers {
                                headers_map.insert(key.into(), Dynamic::from(value));
                            }
                            result.insert("headers".into(), Dynamic::from(headers_map));
                            result.insert("body".into(), Dynamic::from(body_text));
                        }
                        Err(e) => {
                            result.insert("ok".into(), Dynamic::from(false));
                            result.insert("error".into(), Dynamic::from(e));
                        }
                    }
                } else {
                    result.insert("ok".into(), Dynamic::from(false));
                    result.insert("error".into(), Dynamic::from("Network not available"));
                }
            }

            result
        });

        // http.available() -> bool
        engine.register_fn("available", |_: &mut HttpModule| -> bool {
            crate::NET_STATE.lock().is_some()
        });
    }

    /// Create a new runtime (internal, use get_runtime() for cached access)
    fn new_internal() -> Self {
        log_debug!("Initializing JavaScript runtime...");

        let mut engine = Engine::new_raw();

        // Register StandardPackage
        let package = StandardPackage::new();
        engine.register_global_module(package.as_shared_module());
        engine.set_optimization_level(OptimizationLevel::Full);

        // Engine limits
        engine.set_max_call_levels(64);
        engine.set_max_operations(10_000_000);
        engine.set_max_string_size(16384);
        engine.set_max_array_size(10000);
        engine.set_max_map_size(1000);
        engine.set_max_expr_depths(64, 64);

        // Register all module functions as globals
        Self::register_fs_module(&mut engine);
        Self::register_net_module(&mut engine);
        Self::register_sys_module(&mut engine);
        Self::register_mem_module(&mut engine);
        Self::register_http_module(&mut engine);
        Self::register_proc_module(&mut engine);
        Self::register_ipc_module(&mut engine);

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

        engine.register_fn("is_int", |d: Dynamic| -> bool { d.is::<i64>() });

        engine.register_fn("is_float", |d: Dynamic| -> bool { d.is::<f64>() });

        engine.register_fn("is_array", |d: Dynamic| -> bool { d.is::<Array>() });

        engine.register_fn("repeat", |s: ImmutableString, n: i64| -> ImmutableString {
            if n <= 0 {
                return "".into();
            }
            let n = n.min(1000) as usize;
            s.repeat(n).into()
        });

        engine.register_fn(
            "pad_left",
            |s: ImmutableString, width: i64, pad: ImmutableString| -> ImmutableString {
                let width = width.max(0) as usize;
                let pad_char = pad.chars().next().unwrap_or(' ');
                if s.len() >= width {
                    s
                } else {
                    let padding: String =
                        core::iter::repeat(pad_char).take(width - s.len()).collect();
                    format!("{}{}", padding, s).into()
                }
            },
        );

        engine.register_fn(
            "pad_right",
            |s: ImmutableString, width: i64, pad: ImmutableString| -> ImmutableString {
                let width = width.max(0) as usize;
                let pad_char = pad.chars().next().unwrap_or(' ');
                if s.len() >= width {
                    s
                } else {
                    let padding: String =
                        core::iter::repeat(pad_char).take(width - s.len()).collect();
                    format!("{}{}", s, padding).into()
                }
            },
        );

        engine.register_fn(
            "join",
            |arr: Array, sep: ImmutableString| -> ImmutableString {
                let strings: Vec<String> = arr.iter().map(|v| v.to_string()).collect();
                strings.join(sep.as_str()).into()
            },
        );

        engine.register_fn("range", |end: i64| -> Array {
            (0..end.max(0).min(10000)).map(Dynamic::from).collect()
        });

        engine.register_fn("range", |start: i64, end: i64| -> Array {
            let start = start.min(10000);
            let end = end.min(10000);
            (start..end).map(Dynamic::from).collect()
        });

        engine.register_fn("range", |start: i64, end: i64, step: i64| -> Array {
            if step == 0 {
                return Array::new();
            }
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

    /// Execute a script with optional arguments
    /// Uses AST caching for faster repeated execution
    pub fn execute(&self, script: &str, args: &[&str]) -> Result<String, String> {
        log_trace!(
            "Executing script ({} bytes, {} args)",
            script.len(),
            args.len()
        );

        // Preprocess ES6 imports (zero-copy if no imports)
        let preprocess_result = preprocess_imports(script);
        let processed_script = preprocess_result.as_str(script);

        // Compute hash for AST caching
        let script_hash = hash_script(processed_script);

        // Get or compile the AST (cached)
        let ast = get_or_compile_ast(&self.engine, processed_script, script_hash)?;

        init_output();

        // Build scope with arguments
        let mut scope = Scope::new();
        let args_array: Array = args
            .iter()
            .map(|&s| Dynamic::from(ImmutableString::from(s)))
            .collect();
        scope.push("ARGS", args_array);

        // Execute the cached AST
        match self.engine.eval_ast_with_scope::<Dynamic>(&mut scope, &ast) {
            Ok(result) => {
                let output = take_output();
                log_trace!(
                    "Script completed successfully, output: {} bytes",
                    output.len()
                );

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

    /// Execute script without caching (for one-off scripts like REPL)
    pub fn execute_uncached(&self, script: &str, args: &[&str]) -> Result<String, String> {
        log_trace!("Executing script uncached ({} bytes)", script.len());

        let preprocess_result = preprocess_imports(script);
        let processed_script = preprocess_result.as_str(script);

        init_output();

        let mut scope = Scope::new();
        let args_array: Array = args
            .iter()
            .map(|&s| Dynamic::from(ImmutableString::from(s)))
            .collect();
        scope.push("ARGS", args_array);

        match self
            .engine
            .eval_with_scope::<Dynamic>(&mut scope, processed_script)
        {
            Ok(result) => {
                let output = take_output();
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
            Err(e) => Err(format!("Syntax error: {}", e)),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// PUBLIC API
// ═══════════════════════════════════════════════════════════════════════════════

/// Execute a script with arguments (uses cached runtime and AST cache)
pub fn execute_script(script_content: &str, args: &str) -> Result<String, String> {
    let args_vec: Vec<&str> = if args.is_empty() {
        Vec::new()
    } else {
        args.split_whitespace().collect()
    };
    let runtime = get_runtime();
    runtime.execute(script_content, &args_vec)
}

/// Execute a script without AST caching (for REPL/one-off expressions)
pub fn execute_script_uncached(script_content: &str, args: &str) -> Result<String, String> {
    let args_vec: Vec<&str> = if args.is_empty() {
        Vec::new()
    } else {
        args.split_whitespace().collect()
    };
    let runtime = get_runtime();
    runtime.execute_uncached(script_content, &args_vec)
}

pub fn find_script(cmd: &str) -> Option<Vec<u8>> {
    log_trace!("Looking for script: {}", cmd);

    let fs_guard = crate::FS_STATE.lock();
    let mut blk_guard = crate::BLK_DEV.lock();

    if let (Some(fs), Some(dev)) = (fs_guard.as_ref(), blk_guard.as_mut()) {
        if cmd.contains('/') {
            let full_path = if cmd.starts_with('/') {
                alloc::string::String::from(cmd)
            } else {
                crate::resolve_path(cmd)
            };

            log_trace!("Resolved path: {} -> {}", cmd, full_path);

            if let Some(content) = fs.read_file(dev, &full_path) {
                log_debug!(
                    "Found script at path: {} ({} bytes)",
                    full_path,
                    content.len()
                );
                return Some(content);
            }
            log_trace!("Script not found at path: {}", full_path);
            return None;
        }

        let usr_bin_path = format!("/usr/bin/{}", cmd);
        if let Some(content) = fs.read_file(dev, &usr_bin_path) {
            log_debug!(
                "Found script in /usr/bin/: {} ({} bytes)",
                usr_bin_path,
                content.len()
            );
            return Some(content);
        }

        if let Some(content) = fs.read_file(dev, cmd) {
            log_debug!("Found script in root: {} ({} bytes)", cmd, content.len());
            return Some(content);
        }
    }

    log_trace!("Script not found: {}", cmd);
    None
}

pub fn print_info() {
    crate::uart::write_line("");
    crate::uart::write_line(
        "\x1b[1;36m┌─────────────────────────────────────────────────────────────┐\x1b[0m",
    );
    crate::uart::write_line(
        "\x1b[1;36m│\x1b[0m              \x1b[1;97mJavaScript Runtime\x1b[0m                             \x1b[1;36m│\x1b[0m",
    );
    crate::uart::write_line(
        "\x1b[1;36m├─────────────────────────────────────────────────────────────┤\x1b[0m",
    );
    crate::uart::write_line(
        "\x1b[1;36m│\x1b[0m                                                             \x1b[1;36m│\x1b[0m",
    );
    crate::uart::write_line(
        "\x1b[1;36m│\x1b[0m  \x1b[1;33mImport Styles:\x1b[0m                                            \x1b[1;36m│\x1b[0m",
    );
    crate::uart::write_line(
        "\x1b[1;36m│\x1b[0m    import * as fs from \"os:fs\"     \x1b[0;90m// namespace import\x1b[0m    \x1b[1;36m│\x1b[0m",
    );
    crate::uart::write_line(
        "\x1b[1;36m│\x1b[0m    import { ls, read_file } from \"os:fs\"  \x1b[0;90m// named\x1b[0m        \x1b[1;36m│\x1b[0m",
    );
    crate::uart::write_line(
        "\x1b[1;36m│\x1b[0m                                                             \x1b[1;36m│\x1b[0m",
    );
    crate::uart::write_line(
        "\x1b[1;36m│\x1b[0m  \x1b[1;33mModules:\x1b[0m                                                  \x1b[1;36m│\x1b[0m",
    );
    crate::uart::write_line(
        "\x1b[1;36m│\x1b[0m    \x1b[1;32mos:fs\x1b[0m   ls() read(p) write(p,d) exists(p) available()   \x1b[1;36m│\x1b[0m",
    );
    crate::uart::write_line(
        "\x1b[1;36m│\x1b[0m    \x1b[1;32mos:net\x1b[0m  ip() mac() gateway() dns() prefix() available() \x1b[1;36m│\x1b[0m",
    );
    crate::uart::write_line(
        "\x1b[1;36m│\x1b[0m    \x1b[1;32mos:sys\x1b[0m  time() sleep(ms) cwd() version() arch()        \x1b[1;36m│\x1b[0m",
    );
    crate::uart::write_line(
        "\x1b[1;36m│\x1b[0m    \x1b[1;32mos:mem\x1b[0m  total() used() free() stats()                   \x1b[1;36m│\x1b[0m",
    );
    crate::uart::write_line(
        "\x1b[1;36m│\x1b[0m    \x1b[1;32mos:http\x1b[0m get(url) post(url,body,ct) request(opts)      \x1b[1;36m│\x1b[0m",
    );
    crate::uart::write_line(
        "\x1b[1;36m│\x1b[0m                                                             \x1b[1;36m│\x1b[0m",
    );
    crate::uart::write_line(
        "\x1b[1;36m│\x1b[0m  \x1b[1;33mHTTP Response:\x1b[0m  {ok, status, statusText, headers, body}  \x1b[1;36m│\x1b[0m",
    );
    crate::uart::write_line(
        "\x1b[1;36m│\x1b[0m                                                             \x1b[1;36m│\x1b[0m",
    );
    crate::uart::write_line(
        "\x1b[1;36m│\x1b[0m  \x1b[1;33mGlobals:\x1b[0m  print() write() debug() ARGS                    \x1b[1;36m│\x1b[0m",
    );
    crate::uart::write_line(
        "\x1b[1;36m│\x1b[0m            parse_int() parse_float() join() range()...      \x1b[1;36m│\x1b[0m",
    );
    crate::uart::write_line(
        "\x1b[1;36m│\x1b[0m                                                             \x1b[1;36m│\x1b[0m",
    );
    crate::uart::write_line(
        "\x1b[1;36m│\x1b[0m  \x1b[1;33mLimits:\x1b[0m  call_depth=64  ops=1M  strings=16KB  arrays=10K  \x1b[1;36m│\x1b[0m",
    );
    crate::uart::write_line(
        "\x1b[1;36m└─────────────────────────────────────────────────────────────┘\x1b[0m",
    );
    crate::uart::write_line("");
}
