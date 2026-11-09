#![cfg_attr(windows, windows_subsystem = "windows")]

mod graph;
#[cfg(windows)]
mod startup;
#[cfg(windows)]
mod windows_app;

#[cfg(windows)]
fn main() {
    if let Err(error) = windows_app::run() {
        if std::env::args().any(|argument| argument == "--smoke-test") {
            if let Ok(exe) = std::env::current_exe() {
                let path = exe.with_extension("smoke-test.txt");
                let report = std::fs::read_to_string(&path).unwrap_or_default();
                let _ = std::fs::write(path, format!("{report}FAILED: {error}\n"));
            }
            std::process::exit(1);
        }
        windows_app::show_error(&error.to_string());
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("This application requires Windows 10 or later.");
}
