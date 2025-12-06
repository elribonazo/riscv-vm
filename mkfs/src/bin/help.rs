// help - Show available commands and system information
//
// Usage:
//   help              Show all commands
//   help <command>    Show help for specific command

#![cfg_attr(target_arch = "wasm32", no_std)]
#![cfg_attr(target_arch = "wasm32", no_main)]

// Use the lib's panic handler
#[cfg(target_arch = "wasm32")]
extern crate mkfs;

#[cfg(target_arch = "wasm32")]
mod wasm {
    extern "C" {
        fn print(ptr: *const u8, len: usize);
        fn arg_count() -> i32;
        fn arg_get(index: i32, buf_ptr: *mut u8, buf_len: i32) -> i32;
        fn net_available() -> i32;
        fn time() -> i64;
    }

    fn log(s: &str) {
        unsafe { print(s.as_ptr(), s.len()) };
    }

    fn show_command_help(cmd: &[u8]) {
        match cmd {
            b"cd" => {
                log("\x1b[1mcd\x1b[0m - Change directory\n\n");
                log("Usage: cd <directory>\n\n");
                log("Examples:\n");
                log("  cd /home        Go to /home\n");
                log("  cd ..           Go up one level\n");
                log("  cd /            Go to root\n");
            }
            b"ls" => {
                log("\x1b[1mls\x1b[0m - List directory contents\n\n");
                log("Usage: ls [directory]\n\n");
                log("Examples:\n");
                log("  ls              List current directory\n");
                log("  ls /usr/bin     List /usr/bin\n");
            }
            b"cat" => {
                log("\x1b[1mcat\x1b[0m - Display file contents\n\n");
                log("Usage: cat <file>\n\n");
                log("Examples:\n");
                log("  cat /etc/init.d/startup\n");
                log("  cat README.md\n");
            }
            b"echo" => {
                log("\x1b[1mecho\x1b[0m - Print text or write to file\n\n");
                log("Usage:\n");
                log("  echo <text>           Print text\n");
                log("  echo <text> > <file>  Write to file\n\n");
                log("Examples:\n");
                log("  echo Hello World\n");
                log("  echo 'data' > /tmp/test.txt\n");
            }
            b"wget" => {
                log("\x1b[1mwget\x1b[0m - Download files from the web\n\n");
                log("Usage: wget <url> [-O <file>]\n\n");
                log("Options:\n");
                log("  -O <file>  Save to specified file\n\n");
                log("Examples:\n");
                log("  wget https://example.com/file.txt\n");
                log("  wget https://example.com/app.wasm -O /usr/bin/app\n");
            }
            b"pkg" => {
                log("\x1b[1mpkg\x1b[0m - Package manager\n\n");
                log("Usage: pkg <command> [args]\n\n");
                log("Commands:\n");
                log("  list              List installed packages\n");
                log("  install <url>     Install from URL\n");
                log("  info <name>       Show package info\n");
            }
            b"nano" => {
                log("\x1b[1mnano\x1b[0m - Text file viewer\n\n");
                log("Usage: nano <filename>\n\n");
                log("Shows file contents with line numbers.\n");
            }
            b"dmesg" => {
                log("\x1b[1mdmesg\x1b[0m - Display kernel log\n\n");
                log("Usage: dmesg [-n <count>]\n\n");
                log("Options:\n");
                log("  -n <count>  Show last N messages\n");
            }
            b"cowsay" => {
                log("\x1b[1mcowsay\x1b[0m - ASCII art cow\n\n");
                log("Usage: cowsay [message]\n\n");
                log("Examples:\n");
                log("  cowsay             Say 'Moo!'\n");
                log("  cowsay Hello!      Say 'Hello!'\n");
            }
            _ => {
                log("\x1b[31mNo help available for: \x1b[0m");
                unsafe { print(cmd.as_ptr(), cmd.len()) };
                log("\n");
            }
        }
    }

    #[no_mangle]
    pub extern "C" fn _start() {
        let argc = unsafe { arg_count() };
        
        // Check if asking for specific command help
        if argc >= 1 {
            let mut cmd_buf = [0u8; 32];
            let cmd_len = unsafe { arg_get(0, cmd_buf.as_mut_ptr(), 32) };
            if cmd_len > 0 {
                show_command_help(&cmd_buf[..cmd_len as usize]);
                return;
            }
        }
        
        // Show full help
        log("\n");
        log("\x1b[1;36m╔══════════════════════════════════════════════════════════╗\x1b[0m\n");
        log("\x1b[1;36m║\x1b[0m           \x1b[1;37mBAVY OS - Command Reference\x1b[0m                   \x1b[1;36m║\x1b[0m\n");
        log("\x1b[1;36m╚══════════════════════════════════════════════════════════╝\x1b[0m\n\n");
        
        // Built-in Shell Commands
        log("\x1b[1;33m┌─ Built-in Commands ─────────────────────────────────────┐\x1b[0m\n");
        log("\x1b[33m│\x1b[0m  \x1b[1mcd\x1b[0m <dir>      Change directory                        \x1b[33m│\x1b[0m\n");
        log("\x1b[33m│\x1b[0m  \x1b[1mpwd\x1b[0m           Print working directory                  \x1b[33m│\x1b[0m\n");
        log("\x1b[33m│\x1b[0m  \x1b[1mls\x1b[0m [dir]      List directory contents                  \x1b[33m│\x1b[0m\n");
        log("\x1b[33m│\x1b[0m  \x1b[1mcat\x1b[0m <file>    Display file contents                    \x1b[33m│\x1b[0m\n");
        log("\x1b[33m│\x1b[0m  \x1b[1mecho\x1b[0m <text>   Print text (or > file to write)         \x1b[33m│\x1b[0m\n");
        log("\x1b[33m│\x1b[0m  \x1b[1mclear\x1b[0m         Clear the screen                         \x1b[33m│\x1b[0m\n");
        log("\x1b[33m│\x1b[0m  \x1b[1mshutdown\x1b[0m      Power off the system                     \x1b[33m│\x1b[0m\n");
        log("\x1b[33m└──────────────────────────────────────────────────────────┘\x1b[0m\n\n");
        
        // WASM Programs
        log("\x1b[1;32m┌─ Installed Programs ────────────────────────────────────┐\x1b[0m\n");
        log("\x1b[32m│\x1b[0m  \x1b[1mhelp\x1b[0m [cmd]    Show help (this screen)                 \x1b[32m│\x1b[0m\n");
        log("\x1b[32m│\x1b[0m  \x1b[1mdmesg\x1b[0m [-n N]  Display kernel log messages              \x1b[32m│\x1b[0m\n");
        log("\x1b[32m│\x1b[0m  \x1b[1mnano\x1b[0m <file>   View file with line numbers             \x1b[32m│\x1b[0m\n");
        log("\x1b[32m│\x1b[0m  \x1b[1mwget\x1b[0m <url>    Download files from the web             \x1b[32m│\x1b[0m\n");
        log("\x1b[32m│\x1b[0m  \x1b[1mpkg\x1b[0m <cmd>     Package manager                         \x1b[32m│\x1b[0m\n");
        log("\x1b[32m│\x1b[0m  \x1b[1mcowsay\x1b[0m [msg]  ASCII art cow says something            \x1b[32m│\x1b[0m\n");
        log("\x1b[32m│\x1b[0m  \x1b[1mhello\x1b[0m         Test WASM execution                      \x1b[32m│\x1b[0m\n");
        log("\x1b[32m└──────────────────────────────────────────────────────────┘\x1b[0m\n\n");
        
        // System Status
        log("\x1b[1;35m┌─ System Status ─────────────────────────────────────────┐\x1b[0m\n");
        
        // Network status
        let net = unsafe { net_available() };
        if net == 1 {
            log("\x1b[35m│\x1b[0m  Network:      \x1b[32m● Online\x1b[0m                               \x1b[35m│\x1b[0m\n");
        } else {
            log("\x1b[35m│\x1b[0m  Network:      \x1b[31m○ Offline\x1b[0m                              \x1b[35m│\x1b[0m\n");
        }
        
        log("\x1b[35m│\x1b[0m  Kernel:       BAVY RISC-V                            \x1b[35m│\x1b[0m\n");
        log("\x1b[35m│\x1b[0m  Shell:        Built-in                               \x1b[35m│\x1b[0m\n");
        log("\x1b[35m└──────────────────────────────────────────────────────────┘\x1b[0m\n\n");
        
        log("\x1b[90mTip: Run 'help <command>' for detailed help on a command.\x1b[0m\n");
        log("\x1b[90mTip: Use Ctrl+C to cancel a running command.\x1b[0m\n\n");
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {}
