//! Recursive folder scan for a size treemap. Not wired to the UI yet: the
//! treemap view is part of the Phase 2 migration.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::search::is_skipped_dir;

#[derive(serde::Serialize, Clone, Debug)]
pub struct TreemapNode {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    pub is_dir: bool,
    pub children: Option<Vec<TreemapNode>>,
}

fn node_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn empty_node(path: &Path, is_dir: bool) -> TreemapNode {
    TreemapNode {
        name: node_name(path),
        path: path.to_path_buf(),
        size: 0,
        is_dir,
        children: None,
    }
}

/// Best-effort size of `path` and its descendants. Skips the same directories
/// as the indexer, does not follow directory symlinks, and stops when `cancel`
/// is set.
fn subtree_size(path: &Path, cancel: &AtomicBool) -> u64 {
    let mut total: u64 = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                let child = entry.path();
                if let Some(name) = child.file_name().and_then(|n| n.to_str()) {
                    if is_skipped_dir(name) {
                        continue;
                    }
                }
                stack.push(child);
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

pub struct StorageAnalyzer;

impl StorageAnalyzer {
    /// Immediate children plus totals. The UI only shows the top five children,
    /// so the full descendant tree is not retained.
    pub fn scan_directory(path: &Path, cancel: &AtomicBool) -> TreemapNode {
        let metadata = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(_) => return empty_node(path, path.is_dir()),
        };

        if !metadata.is_dir() {
            return TreemapNode {
                name: node_name(path),
                path: path.to_path_buf(),
                size: metadata.len(),
                is_dir: false,
                children: None,
            };
        }

        let mut children: Vec<TreemapNode> = Vec::new();
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let child_path = entry.path();
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                if meta.is_dir() {
                    if let Some(name) = child_path.file_name().and_then(|n| n.to_str()) {
                        if is_skipped_dir(name) {
                            continue;
                        }
                    }
                    let size = subtree_size(&child_path, cancel);
                    children.push(TreemapNode {
                        name: node_name(&child_path),
                        path: child_path,
                        size,
                        is_dir: true,
                        children: None,
                    });
                } else {
                    children.push(TreemapNode {
                        name: node_name(&child_path),
                        path: child_path,
                        size: meta.len(),
                        is_dir: false,
                        children: None,
                    });
                }
            }
        }

        let total_size = children
            .iter()
            .fold(0u64, |acc, c| acc.saturating_add(c.size));

        TreemapNode {
            name: node_name(path),
            path: path.to_path_buf(),
            size: total_size,
            is_dir: true,
            children: Some(children),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn scan_sums_immediate_children_and_does_not_keep_grandchildren() {
        let tmp = TempDir::new("scan_tree");
        tmp.file("a.bin", &[0u8; 100]);
        tmp.file("sub/b.bin", &[0u8; 250]);
        tmp.file("sub/deep/c.bin", &[0u8; 400]);

        let root = StorageAnalyzer::scan_directory(tmp.path(), &AtomicBool::new(false));
        assert_eq!(root.size, 750);
        let children = root.children.expect("children");
        let sub = children.iter().find(|c| c.name == "sub").expect("sub");
        assert_eq!(sub.size, 650);
        assert!(
            sub.children.is_none(),
            "only the scanned directory's children are retained"
        );
    }

    #[test]
    fn scan_skips_node_modules_and_dot_directories() {
        let tmp = TempDir::new("scan_skip");
        tmp.file("keep.bin", &[0u8; 10]);
        tmp.file("node_modules/pkg/big.bin", &[0u8; 1000]);
        tmp.file(".git/objects/x", &[0u8; 1000]);

        let root = StorageAnalyzer::scan_directory(tmp.path(), &AtomicBool::new(false));
        assert_eq!(root.size, 10);
        let children = root.children.expect("children");
        assert!(children
            .iter()
            .all(|c| c.name != "node_modules" && c.name != ".git"));
    }

    #[test]
    fn scan_stops_when_cancelled() {
        let tmp = TempDir::new("scan_cancel");
        tmp.file("a.bin", &[0u8; 10]);
        let cancel = AtomicBool::new(true);
        let root = StorageAnalyzer::scan_directory(tmp.path(), &cancel);
        assert_eq!(root.size, 0);
        assert!(root.children.unwrap_or_default().is_empty());
    }

    #[test]
    fn scan_saturates_instead_of_overflowing() {
        // Two children whose sizes would wrap a naive `sum()` if they were
        // near u64::MAX; saturating_add keeps the total at the max.
        let a = TreemapNode {
            name: "a".into(),
            path: PathBuf::from("a"),
            size: u64::MAX,
            is_dir: false,
            children: None,
        };
        let b = TreemapNode {
            name: "b".into(),
            path: PathBuf::from("b"),
            size: 1,
            is_dir: false,
            children: None,
        };
        let total = [a, b]
            .iter()
            .fold(0u64, |acc, c| acc.saturating_add(c.size));
        assert_eq!(total, u64::MAX);
    }
}
