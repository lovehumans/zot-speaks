//! The single shared stdout writer. Both the protocol thread and the playback thread speak
//! JSON-lines to one stream, serialized by a mutex so frames never interleave.

use std::io::{self, BufWriter, Write};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

pub struct Out {
    w: Mutex<BufWriter<io::Stdout>>,
}

impl Out {
    pub fn new() -> Arc<Self> {
        Arc::new(Out {
            w: Mutex::new(BufWriter::new(io::stdout())),
        })
    }

    pub fn send(&self, frame: &Value) {
        if let Ok(mut w) = self.w.lock() {
            if serde_json::to_writer(w.by_ref(), frame).is_err() {
                return; // zot closed the pipe; the run loop notices on read
            }
            let _ = w.write_all(b"\n");
            let _ = w.flush();
        }
    }

    pub fn notify(&self, level: &str, message: &str) {
        self.send(&json!({"type": "notify", "level": level, "message": message}));
    }
}
