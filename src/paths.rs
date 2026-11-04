//! Paths in the form other programs take. Windows' canonicalize gives verbatim paths (`\\?\C:\x`,
//! `\\?\UNC\server\share\x`) that PHP, `docker` and people cannot use; every path the tool resolves goes through here.

use std::path::{Path, PathBuf};

/// The path made absolute with its links resolved, as `C:\x` (`\\server\share\x`) on Windows.
pub fn real(path: &Path) -> std::io::Result<PathBuf> {
    path.canonicalize().map(plain)
}

/// `C:\x` for `\\?\C:\x`, `\\server\share` for `\\?\UNC\server\share`; any other path as it is.
pub fn plain(path: PathBuf) -> PathBuf {
    match path.to_str() {
        Some(s) if s.starts_with(r"\\?\UNC\") => PathBuf::from(format!(r"\\{}", &s[8..])),
        Some(s) if s.starts_with(r"\\?\") => PathBuf::from(&s[4..]),
        _ => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_paths() {
        assert_eq!(
            plain(PathBuf::from(r"\\?\C:\laragon\www\app")),
            PathBuf::from(r"C:\laragon\www\app")
        );
        assert_eq!(
            plain(PathBuf::from(r"\\?\UNC\server\share\app")),
            PathBuf::from(r"\\server\share\app")
        );
        assert_eq!(
            plain(PathBuf::from("/home/me/app")),
            PathBuf::from("/home/me/app")
        );
        let dir = tempfile::tempdir().unwrap();
        assert!(
            !real(dir.path())
                .unwrap()
                .to_string_lossy()
                .starts_with(r"\\?\")
        );
    }
}
