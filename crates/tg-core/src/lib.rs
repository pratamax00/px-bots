pub mod bot;
pub mod http;
pub mod state;
pub mod ui;

pub fn log_info(msg: &str) {
    eprintln!("[INFO] {msg}");
}

pub fn log_warn(msg: &str) {
    eprintln!("[WARN] {msg}");
}

pub fn log_error(msg: &str) {
    eprintln!("[ERROR] {msg}");
}
