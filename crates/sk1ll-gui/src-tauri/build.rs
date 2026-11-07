fn main() {
    #[cfg(unix)]
    tauri_build::build();
}
