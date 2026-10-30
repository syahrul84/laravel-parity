//! Progress lines. On the command line they go to stdout; inside the MCP server (where stdout carries the protocol)
//! they go to stderr and to the log of the job that is running, which the `status` tool shows.

use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

static SERVER: AtomicBool = AtomicBool::new(false);
static JOB_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn server_mode() {
    SERVER.store(true, Ordering::SeqCst);
}

pub fn say(text: &str) {
    if SERVER.load(Ordering::SeqCst) {
        eprintln!("{text}");
        let mut log = JOB_LOG.lock().unwrap();
        log.push(text.to_string());
        // a long run prints thousands of progress lines; keep the recent ones
        if log.len() > 400 {
            log.drain(..200);
        }
    } else {
        println!("{text}");
        let _ = std::io::stdout().flush();
    }
}

pub fn clear() {
    JOB_LOG.lock().unwrap().clear();
}

pub fn tail(lines: usize) -> Vec<String> {
    let log = JOB_LOG.lock().unwrap();
    log[log.len().saturating_sub(lines)..].to_vec()
}
