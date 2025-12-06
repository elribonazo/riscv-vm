use alloc::{format, string::String, vec, vec::Vec};
use wasmi::{Caller, Engine, Func, Linker, Module, Store};

use crate::uart;

/// State to pass to host functions - includes command arguments
struct WasmContext {
    args: Vec<String>,
}

/// Execute a WASM binary with the given arguments
pub fn execute(wasm_bytes: &[u8], args: &[&str]) -> Result<String, String> {
    let engine = Engine::default();
    let ctx = WasmContext {
        args: args.iter().map(|s| String::from(*s)).collect(),
    };
    let mut store = Store::new(&engine, ctx);
    let mut linker = Linker::new(&engine);

    // Syscall: print(ptr, len)
    linker
        .define(
            "env",
            "print",
            Func::wrap(
                &mut store,
                |caller: Caller<'_, WasmContext>, ptr: i32, len: i32| {
                    if let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) {
                        let mut buffer = vec![0u8; len as usize];
                        if mem.read(&caller, ptr as usize, &mut buffer).is_ok() {
                            uart::write_str(&String::from_utf8_lossy(&buffer));
                        }
                    }
                },
            ),
        )
        .map_err(|e| format!("define print: {:?}", e))?;

    // Syscall: time() -> i64
    linker
        .define(
            "env",
            "time",
            Func::wrap(&mut store, |_caller: Caller<'_, WasmContext>| -> i64 {
                crate::get_time_ms()
            }),
        )
        .map_err(|e| format!("define time: {:?}", e))?;

    // Syscall: arg_count() -> i32
    linker
        .define(
            "env",
            "arg_count",
            Func::wrap(&mut store, |caller: Caller<'_, WasmContext>| -> i32 {
                caller.data().args.len() as i32
            }),
        )
        .map_err(|e| format!("define arg_count: {:?}", e))?;

    // Syscall: arg_get(index, buf_ptr, buf_len) -> i32
    linker
        .define(
            "env",
            "arg_get",
            Func::wrap(
                &mut store,
                |mut caller: Caller<'_, WasmContext>,
                 index: i32,
                 buf_ptr: i32,
                 buf_len: i32|
                 -> i32 {
                    // Clone the arg to avoid borrow issues
                    let arg_opt = {
                        let args = &caller.data().args;
                        if index < 0 || (index as usize) >= args.len() {
                            None
                        } else {
                            Some(args[index as usize].clone())
                        }
                    };

                    if let Some(arg) = arg_opt {
                        let bytes = arg.as_bytes();
                        if bytes.len() > buf_len as usize {
                            return -1;
                        }
                        if let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory())
                        {
                            if mem.write(&mut caller, buf_ptr as usize, bytes).is_ok() {
                                return bytes.len() as i32;
                            }
                        }
                    }
                    -1
                },
            ),
        )
        .map_err(|e| format!("define arg_get: {:?}", e))?;

    // Syscall: cwd_get(buf_ptr, buf_len) -> i32
    linker
        .define(
            "env",
            "cwd_get",
            Func::wrap(
                &mut store,
                |mut caller: Caller<'_, WasmContext>, buf_ptr: i32, buf_len: i32| -> i32 {
                    let cwd = crate::cwd_get();
                    let bytes = cwd.as_bytes();
                    if bytes.len() > buf_len as usize {
                        return -1;
                    }
                    if let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) {
                        if mem.write(&mut caller, buf_ptr as usize, bytes).is_ok() {
                            return bytes.len() as i32;
                        }
                    }
                    -1
                },
            ),
        )
        .map_err(|e| format!("define cwd_get: {:?}", e))?;

    // Syscall: fs_exists(path_ptr, path_len) -> i32
    linker
        .define(
            "env",
            "fs_exists",
            Func::wrap(
                &mut store,
                |caller: Caller<'_, WasmContext>, path_ptr: i32, path_len: i32| -> i32 {
                    if let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) {
                        let mut path_buf = vec![0u8; path_len as usize];
                        if mem.read(&caller, path_ptr as usize, &mut path_buf).is_ok() {
                            if let Ok(path) = core::str::from_utf8(&path_buf) {
                                let fs_guard = crate::FS_STATE.lock();
                                let mut blk_guard = crate::BLK_DEV.lock();
                                if let (Some(fs), Some(dev)) =
                                    (fs_guard.as_ref(), blk_guard.as_mut())
                                {
                                    return if fs.read_file(dev, path).is_some() {
                                        1
                                    } else {
                                        0
                                    };
                                }
                            }
                        }
                    }
                    0
                },
            ),
        )
        .map_err(|e| format!("define fs_exists: {:?}", e))?;

    // Syscall: fs_read(path_ptr, path_len, buf_ptr, buf_len) -> i32
    linker
        .define(
            "env",
            "fs_read",
            Func::wrap(
                &mut store,
                |mut caller: Caller<'_, WasmContext>,
                 path_ptr: i32,
                 path_len: i32,
                 buf_ptr: i32,
                 buf_len: i32|
                 -> i32 {
                    if let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) {
                        let mut path_buf = vec![0u8; path_len as usize];
                        if mem.read(&caller, path_ptr as usize, &mut path_buf).is_ok() {
                            if let Ok(path) = core::str::from_utf8(&path_buf) {
                                let fs_guard = crate::FS_STATE.lock();
                                let mut blk_guard = crate::BLK_DEV.lock();
                                if let (Some(fs), Some(dev)) =
                                    (fs_guard.as_ref(), blk_guard.as_mut())
                                {
                                    if let Some(data) = fs.read_file(dev, path) {
                                        let to_copy = data.len().min(buf_len as usize);
                                        if mem
                                            .write(&mut caller, buf_ptr as usize, &data[..to_copy])
                                            .is_ok()
                                        {
                                            return to_copy as i32;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    -1
                },
            ),
        )
        .map_err(|e| format!("define fs_read: {:?}", e))?;

    // Syscall: fs_write(path_ptr, path_len, data_ptr, data_len) -> i32
    linker
        .define(
            "env",
            "fs_write",
            Func::wrap(
                &mut store,
                |caller: Caller<'_, WasmContext>,
                 path_ptr: i32,
                 path_len: i32,
                 data_ptr: i32,
                 data_len: i32|
                 -> i32 {
                    if let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) {
                        let mut path_buf = vec![0u8; path_len as usize];
                        let mut data_buf = vec![0u8; data_len as usize];
                        if mem.read(&caller, path_ptr as usize, &mut path_buf).is_ok()
                            && mem.read(&caller, data_ptr as usize, &mut data_buf).is_ok()
                        {
                            if let Ok(path) = core::str::from_utf8(&path_buf) {
                                let mut fs_guard = crate::FS_STATE.lock();
                                let mut blk_guard = crate::BLK_DEV.lock();
                                if let (Some(fs), Some(dev)) =
                                    (fs_guard.as_mut(), blk_guard.as_mut())
                                {
                                    if fs.write_file(dev, path, &data_buf).is_ok() {
                                        return data_len;
                                    }
                                }
                            }
                        }
                    }
                    -1
                },
            ),
        )
        .map_err(|e| format!("define fs_write: {:?}", e))?;

    // Syscall: fs_list(buf_ptr, buf_len) -> i32
    linker
        .define(
            "env",
            "fs_list",
            Func::wrap(
                &mut store,
                |mut caller: Caller<'_, WasmContext>, buf_ptr: i32, buf_len: i32| -> i32 {
                    let mut fs_guard = crate::FS_STATE.lock();
                    let mut blk_guard = crate::BLK_DEV.lock();
                    if let (Some(fs), Some(dev)) = (fs_guard.as_mut(), blk_guard.as_mut()) {
                        let files = fs.list_dir(dev, "/");
                        // Format as simple newline-separated list: "name:size\n"
                        let mut output = String::new();
                        for file in files {
                            output.push_str(&file.name);
                            output.push(':');
                            output.push_str(&format!("{}", file.size));
                            output.push('\n');
                        }
                        let bytes = output.as_bytes();
                        if bytes.len() > buf_len as usize {
                            return -1;
                        }
                        if let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory())
                        {
                            if mem.write(&mut caller, buf_ptr as usize, bytes).is_ok() {
                                return bytes.len() as i32;
                            }
                        }
                    }
                    -1
                },
            ),
        )
        .map_err(|e| format!("define fs_list: {:?}", e))?;

    // Syscall: klog_get(count, buf_ptr, buf_len) -> i32
    linker
        .define(
            "env",
            "klog_get",
            Func::wrap(
                &mut store,
                |mut caller: Caller<'_, WasmContext>,
                 count: i32,
                 buf_ptr: i32,
                 buf_len: i32|
                 -> i32 {
                    let count = (count as usize).max(1).min(100);
                    let entries = crate::klog::KLOG.recent(count);
                    let mut output = String::new();
                    for entry in entries.iter().rev() {
                        output.push_str(&entry.format_colored());
                        output.push('\n');
                    }
                    let bytes = output.as_bytes();
                    if bytes.len() > buf_len as usize {
                        return -1;
                    }
                    if let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) {
                        if mem.write(&mut caller, buf_ptr as usize, bytes).is_ok() {
                            return bytes.len() as i32;
                        }
                    }
                    -1
                },
            ),
        )
        .map_err(|e| format!("define klog_get: {:?}", e))?;

    // Syscall: net_available() -> i32
    linker
        .define(
            "env",
            "net_available",
            Func::wrap(&mut store, |_caller: Caller<'_, WasmContext>| -> i32 {
                let net_guard = crate::NET_STATE.lock();
                if net_guard.is_some() {
                    1
                } else {
                    0
                }
            }),
        )
        .map_err(|e| format!("define net_available: {:?}", e))?;

    // Syscall: http_get(url_ptr, url_len, resp_ptr, resp_len) -> i32
    linker
        .define(
            "env",
            "http_get",
            Func::wrap(
                &mut store,
                |mut caller: Caller<'_, WasmContext>,
                 url_ptr: i32,
                 url_len: i32,
                 resp_ptr: i32,
                 resp_len: i32|
                 -> i32 {
                    if let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) {
                        let mut url_buf = vec![0u8; url_len as usize];
                        if mem.read(&caller, url_ptr as usize, &mut url_buf).is_ok() {
                            if let Ok(url) = core::str::from_utf8(&url_buf) {
                                let mut net_guard = crate::NET_STATE.lock();
                                if let Some(ref mut net) = *net_guard {
                                    match crate::http::get_follow_redirects(
                                        net,
                                        url,
                                        30000,
                                        crate::get_time_ms,
                                    ) {
                                        Ok(response) => {
                                            // Return just the body (already Vec<u8>)
                                            let bytes = &response.body;
                                            let to_copy = bytes.len().min(resp_len as usize);
                                            if mem
                                                .write(
                                                    &mut caller,
                                                    resp_ptr as usize,
                                                    &bytes[..to_copy],
                                                )
                                                .is_ok()
                                            {
                                                return to_copy as i32;
                                            }
                                        }
                                        Err(_) => return -1,
                                    }
                                }
                            }
                        }
                    }
                    -1
                },
            ),
        )
        .map_err(|e| format!("define http_get: {:?}", e))?;

    let module = Module::new(&engine, wasm_bytes).map_err(|e| format!("Invalid WASM: {:?}", e))?;

    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|e| format!("Link: {:?}", e))?
        .start(&mut store)
        .map_err(|e| format!("Start: {:?}", e))?;

    let run = instance
        .get_typed_func::<(), ()>(&store, "_start")
        .map_err(|e| format!("Missing _start: {:?}", e))?;

    run.call(&mut store, ())
        .map_err(|e| format!("Runtime: {:?}", e))?;

    Ok(String::new())
}
