fn main() {
    // Delayload linking is only meaningful on Windows targets. The build
    // script itself runs on the HOST, so cfg(windows) is the wrong gate
    // when cross-compiling from macOS: inspect the target triple instead.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winfsp::build::winfsp_link_delayload();
    }
}
