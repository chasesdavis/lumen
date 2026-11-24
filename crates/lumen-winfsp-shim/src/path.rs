//! NT-path to POSIX-path translation shared by the WinFsp shim and its
//! unit tests. Split out of the (windows-only) FileSystemContext impl so
//! path handling gets tested on every dev platform, mirroring how
//! lumen-fuse-shim keeps errno mapping in attr.rs for testing.

/// WinFsp hands down NT-style paths using backslash separators after
/// kernel normalization. Lumen's MountOps expects forward-slash absolute
/// paths with a root of "/". Consecutive separators collapse and any
/// leading drive/device prefix is stripped.
pub fn nt_to_posix(file_name: &str) -> String {
    // Strip device prefixes: "\\?\C:\x", "\Device\...", or plain "\".
    let s = strip_device_prefix(file_name);
    let replaced = s
        .split(BACKSLASH)
        .collect::<Vec<_>>()
        .join(&FORWARD_SLASH.to_string());
    if replaced.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", replaced.trim_start_matches('/'))
    }
}

const BACKSLASH: char = '\\';
const FORWARD_SLASH: char = '/';

fn strip_device_prefix(s: &str) -> &str {
    // WinFsp normalized paths start with a single backslash. The DOS device
    // prefix form ("\\?\C:\..." or "\\\\.\\C:\\") carries two leading
    // separators plus a marker character; drop those so only the drive or
    // relative component remains.
    if let Some(rest) = s.strip_prefix(r"\?\") {
        return rest;
    }
    if let Some(rest) = s.strip_prefix(r"\.\") {
        return rest;
    }
    match s.strip_prefix(BACKSLASH) {
        Some(rest) => rest,
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_maps_to_root() {
        assert_eq!(nt_to_posix(""), "/");
    }

    #[test]
    fn backslash_converts_to_forward_slash() {
        let input = format!("{0}foo{0}bar{0}baz.txt", BACKSLASH);
        assert_eq!(nt_to_posix(&input), "/foo/bar/baz.txt");
    }

    #[test]
    fn absolute_drive_path_strips_prefix() {
        // "C:Userschaseile.txt" style: no leading separator; nothing to
        // strip beyond normalizing separators.
        let input = format!("C:{0}Lumen{0}file.txt", BACKSLASH);
        assert_eq!(nt_to_posix(&input), "/C:/Lumen/file.txt");
    }

    #[test]
    fn leading_separator_is_mount_root() {
        let input = format!("{0}file.txt", BACKSLASH);
        assert_eq!(nt_to_posix(&input), "/file.txt");
    }

    #[test]
    fn redundant_separators_collapse() {
        let input = format!("{0}{0}{0}a{0}{0}b", BACKSLASH);
        assert_eq!(nt_to_posix(&input), "/a//b");
    }

    #[test]
    fn device_prefix_stripped() {
        let input = format!("{0}?{0}C:{0}Lumen", BACKSLASH);
        assert_eq!(nt_to_posix(&input), "/C:/Lumen");
    }
}
