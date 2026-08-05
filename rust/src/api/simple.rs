#[flutter_rust_bridge::frb(sync)] // Synchronous mode for simplicity of the demo
pub fn greet(name: String) -> String {
    format!("Hello, {name}!")
}

#[flutter_rust_bridge::frb(init)]
pub fn init_app() {
    // Do not use setup_default_user_utils(): it sets Trace and floods Android
    // logcat with alacritty_terminal per-CSI traces during CLI TUI redraws.
    flutter_rust_bridge::setup_log_to_console(log::LevelFilter::Warn);
    flutter_rust_bridge::setup_backtrace();
}
