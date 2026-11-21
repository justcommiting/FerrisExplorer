use std::path::{Path, PathBuf};

pub fn get_home_dir() -> PathBuf {
    if cfg!(target_os = "windows") {
        std::env::var("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("C:\\"))
    } else {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/"))
    }
}

pub fn get_drives() -> Vec<PathBuf> {
    if cfg!(target_os = "windows") {
        let mut drives = Vec::new();
        for b in b'A'..=b'Z' {
            let path = format!("{}:\\", b as char);
            if std::path::Path::new(&path).exists() {
                drives.push(PathBuf::from(path));
            }
        }
        drives
    } else if cfg!(target_os = "macos") {
        let mut drives = vec![PathBuf::from("/")];
        if let Ok(entries) = std::fs::read_dir("/Volumes") {
            for entry in entries.flatten() {
                drives.push(entry.path());
            }
        }
        dedupe_by_device(drives, device_id)
    } else {
        // Assume Linux or other Unix
        let mut drives = vec![PathBuf::from("/")];

        // Scan common mount point directories
        let mnt_dirs = ["/mnt", "/media"];
        for dir in mnt_dirs {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        drives.push(path);
                    }
                }
            }
        }

        // Also could include /run/media/$USER if we wanted to be exhaustive, but this covers most standard cases
        if let Ok(user) = std::env::var("USER") {
            let run_media = format!("/run/media/{}", user);
            if let Ok(entries) = std::fs::read_dir(&run_media) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        drives.push(path);
                    }
                }
            }
        }

        dedupe_by_device(drives, device_id)
    }
}

/// Remove any root that sits on a device already covered by an earlier root,
/// keeping the first occurrence. On every modern Mac, `/Volumes` contains an
/// entry for the boot volume mounted a *second* time (`Macintosh HD` here is
/// a symlink to `/`, both reporting the same device id) -- without this,
/// `get_drives()` handed the indexer two roots for the same disk, and
/// `index_multiple_to_sqlite` walked (and inserted) the entire boot volume
/// twice, roughly doubling how long a full index takes.
fn dedupe_by_device(paths: Vec<PathBuf>, device_of: impl Fn(&Path) -> Option<u64>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    paths
        .into_iter()
        .filter(|p| match device_of(p) {
            Some(dev) => seen.insert(dev),
            // Can't stat it (permissions, a race) -- keep it rather than
            // silently dropping a root the walker might still be able to
            // read, or report a clear error for.
            None => true,
        })
        .collect()
}

#[cfg(unix)]
fn device_id(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.dev())
}

#[cfg(not(unix))]
fn device_id(_path: &Path) -> Option<u64> {
    None
}

pub fn reveal_in_finder(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg("/select,")
            .arg(path)
            .spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        let parent = path.parent().unwrap_or(path);
        std::process::Command::new("xdg-open").arg(parent).spawn()?;
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_by_device_drops_a_later_root_on_a_device_already_seen() {
        // Mirrors `/` and `/Volumes/Macintosh HD` sharing a device id.
        let devices = [
            ("/".to_string(), 1u64),
            ("/Volumes/Macintosh HD".to_string(), 1u64),
        ];
        let drives = vec![PathBuf::from("/"), PathBuf::from("/Volumes/Macintosh HD")];

        let result = dedupe_by_device(drives, |p| {
            devices
                .iter()
                .find(|(name, _)| Path::new(name) == p)
                .map(|(_, dev)| *dev)
        });

        assert_eq!(result, vec![PathBuf::from("/")]);
    }

    #[test]
    fn dedupe_by_device_keeps_genuinely_different_volumes() {
        let devices = [
            ("/".to_string(), 1u64),
            ("/Volumes/Backup".to_string(), 2u64),
            ("/Volumes/Media".to_string(), 3u64),
        ];
        let drives = vec![
            PathBuf::from("/"),
            PathBuf::from("/Volumes/Backup"),
            PathBuf::from("/Volumes/Media"),
        ];

        let result = dedupe_by_device(drives.clone(), |p| {
            devices
                .iter()
                .find(|(name, _)| Path::new(name) == p)
                .map(|(_, dev)| *dev)
        });

        assert_eq!(result, drives);
    }

    #[test]
    fn dedupe_by_device_keeps_a_root_it_cannot_stat() {
        let drives = vec![PathBuf::from("/"), PathBuf::from("/nope")];
        let result = dedupe_by_device(drives.clone(), |p| {
            if p == Path::new("/") {
                Some(1)
            } else {
                None // unreadable / doesn't exist
            }
        });
        assert_eq!(result, drives);
    }
}
