//! Socket path helpers shared by the daemon and the doctor.
//!
//! T5-split: `default_socket_path` lived in `kod-core/src/serve.rs`,
//! but `doctor.rs` (which stays in kod-core) needs it too. Moving it
//! here lets both kod-core and the future kod-core-serve depend on
//! kod-core-state for the path without a circular dependency.

use std::path::PathBuf;

pub fn default_socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        if dir.is_dir() {
            return dir.join("kod.sock");
        }
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".kod").join("run").join("kod.sock")
}
