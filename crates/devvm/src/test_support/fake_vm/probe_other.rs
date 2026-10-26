//! The fake VM has nothing to report off Windows: it cannot be spawned frozen there.

pub fn kill_on_close() -> &'static str {
    "n/a"
}

pub fn console_processes() -> String {
    "n/a".to_owned()
}

pub fn console_window() -> &'static str {
    "n/a"
}
