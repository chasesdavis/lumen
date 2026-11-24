fn main() {
    // Delayload linking is only meaningful on Windows targets; on Unix this
    // build script intentionally does nothing so the crate stays checkable.
    #[cfg(windows)]
    winfsp::build::winfsp_link_delayload();
}
