//! Install saves into Brickadia's Proton prefix (Worlds / Prefabs).
//!
//! Shared by egui Map/Grid/Sculpt and the Tauri Convert shell. No egui types.

use std::path::{Path, PathBuf};

/// Brickadia Steam APPID (Proton compatdata).
pub const BRICKADIA_APP_ID: u32 = 2199420;

/// Copy a written save into Brickadia's Saved tree.
///
/// Extension is taken from `path` (`.brdb` → `Worlds/`, `.brz` → `Prefabs/`).
/// `overwrite` writes `<stem>.<ext>` in place; otherwise suffixes `-2`, `-3`, …
/// so a hand-authored world is never clobbered.
pub fn install_save(path: &Path, overwrite: bool) -> Result<PathBuf, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .ok_or_else(|| {
            "output path has no extension (expected .brdb or .brz)".to_owned()
        })?
        .to_ascii_lowercase();
    install_save_ext(path, &ext, overwrite)
}

/// Same as [`install_save`] but with an explicit extension (egui paths that
/// already know brdb vs brz).
pub fn install_save_ext(path: &Path, ext: &str, overwrite: bool) -> Result<PathBuf, String> {
    let dir = saved_subdir(ext)?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("create install dir: {e}"))?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "output path has no file name".to_owned())?;
    let dest = if overwrite {
        dir.join(format!("{stem}.{ext}"))
    } else {
        unique_save_path(&dir, stem, ext)?
    };
    std::fs::copy(path, &dest).map_err(|e| format!("copy to {}: {e}", dest.display()))?;
    Ok(dest)
}

/// First non-colliding `<stem>.<ext>` / `<stem>-N.<ext>` path in `dir`.
/// Bounded loop: up to 1000 attempts.
pub fn unique_save_path(dir: &Path, stem: &str, ext: &str) -> Result<PathBuf, String> {
    let first = dir.join(format!("{stem}.{ext}"));
    if !first.exists() {
        return Ok(first);
    }
    for n in 2..=1000 {
        let candidate = dir.join(format!("{stem}-{n}.{ext}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "too many worlds named '{stem}' already installed (tried -2…-1000) — choose a different output name"
    ))
}

/// Staging directory for generated saves (`~/.local/share/heightmap2brz/builds`).
pub fn builds_dir() -> Result<PathBuf, String> {
    let base = dirs::data_dir().ok_or_else(|| "no XDG data directory".to_owned())?;
    Ok(base.join("heightmap2brz").join("builds"))
}

/// `…/Saved/Worlds` under the Brickadia Proton prefix (diagnostics / default install).
pub fn brickadia_worlds_dir() -> Result<PathBuf, String> {
    Ok(brickadia_saved_dir()?.join("Worlds"))
}

/// Root of Brickadia's `Saved` tree inside the Steam Proton prefix.
pub fn brickadia_saved_dir() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or_else(|| {
        format!(
            "Brickadia Proton prefix not found — no home directory (APPID {BRICKADIA_APP_ID})"
        )
    })?;
    let prefix = home
        .join(".steam/steam/steamapps/compatdata")
        .join(BRICKADIA_APP_ID.to_string())
        .join("pfx/drive_c/users/steamuser/AppData/Local/Brickadia/Saved");
    if !prefix.exists() {
        return Err(format!(
            "Brickadia Proton prefix not found at {} — launch Brickadia at least once",
            prefix.display()
        ));
    }
    Ok(prefix)
}

fn saved_subdir(ext: &str) -> Result<PathBuf, String> {
    match ext {
        "brdb" => Ok(brickadia_saved_dir()?.join("Worlds")),
        "brz" => Ok(brickadia_saved_dir()?.join("Prefabs")),
        other => Err(format!(
            "no Brickadia install path for .{other} saves (only .brdb→Worlds/, .brz→Prefabs/)"
        )),
    }
}

/// True when `err` is the soft "prefix missing" case (convert should still succeed).
pub fn is_prefix_missing(err: &str) -> bool {
    err.contains("Brickadia Proton prefix not found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_save_path_never_collides_with_existing() {
        let dir = std::env::temp_dir().join(format!(
            "bwt-install-unique-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");

        let p0 = unique_save_path(&dir, "mtfuji", "brdb").expect("no collision");
        assert_eq!(p0, dir.join("mtfuji.brdb"));

        std::fs::write(dir.join("mtfuji.brdb"), b"x").unwrap();
        let p1 = unique_save_path(&dir, "mtfuji", "brdb").expect("-2");
        assert_eq!(p1, dir.join("mtfuji-2.brdb"));

        std::fs::write(dir.join("mtfuji-2.brdb"), b"x").unwrap();
        let p2 = unique_save_path(&dir, "mtfuji", "brdb").expect("-3");
        assert_eq!(p2, dir.join("mtfuji-3.brdb"));
        assert!(!p2.exists(), "returned path must not already exist");

        let pbrz = unique_save_path(&dir, "mtfuji", "brz").expect("brz bare");
        assert_eq!(pbrz, dir.join("mtfuji.brz"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn install_save_soft_errors_when_prefix_missing() {
        // If the real prefix exists on this machine, skip the "missing" branch
        // and only assert install_save_ext routing for a fake ext error.
        if brickadia_saved_dir().is_ok() {
            let err = install_save_ext(Path::new("/tmp/x.foo"), "foo", false).unwrap_err();
            assert!(err.contains("no Brickadia install path"), "{err}");
            return;
        }
        let tmp = std::env::temp_dir().join(format!(
            "bwt-install-src-{}",
            std::process::id()
        ));
        std::fs::write(&tmp, b"not-a-real-brdb").unwrap();
        let err = install_save(&tmp, false).expect_err("prefix should be missing");
        assert!(is_prefix_missing(&err), "expected prefix-missing soft error, got: {err}");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn builds_dir_is_under_xdg_data() {
        let dir = builds_dir().expect("builds_dir");
        assert!(dir.ends_with("heightmap2brz/builds") || dir.ends_with("heightmap2brz\\builds"));
    }

    #[test]
    fn install_save_detects_ext_from_path() {
        // Without a real prefix this fails soft; with prefix it would try copy.
        // Only assert extension detection path rejects bare names.
        let err = install_save(Path::new("no_extension"), false).unwrap_err();
        assert!(
            err.contains("no extension") || is_prefix_missing(&err),
            "{err}"
        );
    }
}
