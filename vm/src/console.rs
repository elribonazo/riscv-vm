use std::io::{self, Read};
use std::sync::mpsc;
use std::thread;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct Console {
    rx: mpsc::Receiver<u8>,
    original_termios: Option<libc::termios>,
    running: Arc<AtomicBool>,
}

impl Console {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        let running = Arc::new(AtomicBool::new(true));
        let r_clone = running.clone();

        // Setup raw mode
        let mut original_termios = None;
        if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
            let mut termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut termios) } == 0 {
                original_termios = Some(termios);
                let mut raw = termios;
                unsafe {
                    libc::cfmakeraw(&mut raw);
                    libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw);
                }
            }
        }

        thread::spawn(move || {
            let mut buf = [0u8; 1];
            let stdin = io::stdin();
            let mut handle = stdin.lock();
            
            while r_clone.load(Ordering::Relaxed) {
                // This read is blocking. 
                // In a real app we might want non-blocking or select, 
                // but for this simple VM, blocking thread is fine.
                if handle.read_exact(&mut buf).is_ok() {
                    let b = buf[0];
                    // Translate Ctrl-A x to exit? 
                    // Let's leave exit handling to the consumer or special key.
                    // Typically Ctrl-A x is (1, 120).
                    // We just pass raw bytes.
                    if tx.send(b).is_err() {
                        break;
                    }
                } else {
                    break;
                }
            }
        });

        Self {
            rx,
            original_termios,
            running,
        }
    }

    pub fn poll(&self) -> Option<u8> {
        self.rx.try_recv().ok()
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(termios) = self.original_termios {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &termios);
            }
        }
    }
}

