#[cfg(unix)]
fn main() {
    sk1ll_gui::run();
}

#[cfg(not(unix))]
fn main() {
    eprintln!("sk1ll-gui requires Unix, like sk1ll-daemon");
    std::process::exit(1);
}
