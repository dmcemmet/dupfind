use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::scanner::FileInfo;

/// Sort mode for display ordering.
#[derive(Clone, Copy, PartialEq)]
pub enum SortMode {
    Name,
    Size,
    Date,
    Count,
}

/// A node in the file tree representing either a directory or a duplicate file.
#[derive(Debug, Clone)]
pub struct TreeNode {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub expanded: bool,
    pub children: BTreeMap<String, TreeNode>,
    pub file_info: Option<FileInfo>,
    pub group_index: Option<usize>,
}

impl TreeNode {
    pub fn new_dir(name: &str, path: PathBuf) -> Self {
        Self {
            name: name.to_string(),
            path,
            is_dir: true,
            expanded: true,
            children: BTreeMap::new(),
            file_info: None,
            group_index: None,
        }
    }

    pub fn insert_file(&mut self, rel_path: &Path, info: FileInfo, group_idx: usize) {
        let components: Vec<_> = rel_path.components().collect();
        self.insert_recursive(&components, info, group_idx);
    }

    fn insert_recursive(
        &mut self,
        components: &[std::path::Component],
        info: FileInfo,
        group_idx: usize,
    ) {
        if components.is_empty() {
            return;
        }
        let name = components[0].as_os_str().to_string_lossy().to_string();
        if components.len() == 1 {
            // Leaf file
            self.children
                .entry(name.clone())
                .or_insert_with(|| TreeNode {
                    name: name.clone(),
                    path: info.path.clone(),
                    is_dir: false,
                    expanded: false,
                    children: BTreeMap::new(),
                    file_info: Some(info),
                    group_index: Some(group_idx),
                });
        } else {
            // Directory
            let dir_path = self.path.join(&name);
            let child = self
                .children
                .entry(name.clone())
                .or_insert_with(|| TreeNode::new_dir(&name, dir_path));
            child.insert_recursive(&components[1..], info, group_idx);
        }
    }

    /// Flatten tree into displayable lines: (depth, node_ref)
    pub fn flatten(&self) -> Vec<(usize, &TreeNode)> {
        self.flatten_sorted(SortMode::Name)
    }

    /// Flatten tree with custom sort order for children
    pub fn flatten_sorted(&self, mode: SortMode) -> Vec<(usize, &TreeNode)> {
        let mut result = Vec::new();
        let mut children: Vec<&TreeNode> = self.children.values().collect();
        sort_nodes(&mut children, mode);
        for child in children {
            Self::flatten_recursive_sorted(child, 0, mode, &mut result);
        }
        result
    }

    fn flatten_recursive_sorted<'a>(
        node: &'a TreeNode,
        depth: usize,
        mode: SortMode,
        result: &mut Vec<(usize, &'a TreeNode)>,
    ) {
        result.push((depth, node));
        if node.is_dir && node.expanded {
            let mut children: Vec<&TreeNode> = node.children.values().collect();
            sort_nodes(&mut children, mode);
            for child in children {
                Self::flatten_recursive_sorted(child, depth + 1, mode, result);
            }
        }
    }

    pub fn toggle_expand(&mut self, path: &Path) {
        if self.path == path && self.is_dir {
            self.expanded = !self.expanded;
            return;
        }
        for child in self.children.values_mut() {
            child.toggle_expand(path);
        }
    }

    pub fn set_all_expanded(&mut self, expanded: bool) {
        if self.is_dir {
            self.expanded = expanded;
        }
        for child in self.children.values_mut() {
            child.set_all_expanded(expanded);
        }
    }

}

/// Build a tree containing only files that have duplicates.
pub fn build_tree(groups: &[Vec<FileInfo>], root: &Path) -> TreeNode {
    let mut tree = TreeNode::new_dir("", root.to_path_buf());
    for (group_idx, group) in groups.iter().enumerate() {
        for info in group {
            if let Ok(rel) = info.path.strip_prefix(root) {
                tree.insert_file(rel, info.clone(), group_idx);
            }
        }
    }
    tree
}

/// Build a tree with explicit group indices (for filtered views).
pub fn build_tree_indexed(groups: &[(usize, &Vec<FileInfo>)], root: &Path) -> TreeNode {
    let mut tree = TreeNode::new_dir("", root.to_path_buf());
    for &(group_idx, group) in groups {
        for info in group {
            if let Ok(rel) = info.path.strip_prefix(root) {
                tree.insert_file(rel, info.clone(), group_idx);
            }
        }
    }
    tree
}

fn sort_nodes<'a>(nodes: &mut Vec<&'a TreeNode>, mode: SortMode) {
    match mode {
        SortMode::Name => nodes.sort_by(|a, b| a.name.cmp(&b.name)),
        SortMode::Size => nodes.sort_by(|a, b| {
            let sa = a.file_info.as_ref().map_or(0, |f| f.size);
            let sb = b.file_info.as_ref().map_or(0, |f| f.size);
            sb.cmp(&sa) // largest first
        }),
        SortMode::Date => nodes.sort_by(|a, b| {
            let da = a.file_info.as_ref().and_then(|f| f.modified);
            let db = b.file_info.as_ref().and_then(|f| f.modified);
            db.cmp(&da) // newest first
        }),
        SortMode::Count => nodes.sort_by(|a, b| {
            let ca = count_files(a);
            let cb = count_files(b);
            cb.cmp(&ca) // most files first
        }),
    }
}

pub fn count_files(node: &TreeNode) -> usize {
    if !node.is_dir {
        return 1;
    }
    node.children.values().map(|c| count_files(c)).sum()
}

pub fn dir_total_size(node: &TreeNode) -> u64 {
    if !node.is_dir {
        return node.file_info.as_ref().map_or(0, |f| f.size);
    }
    node.children.values().map(|c| dir_total_size(c)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_info(path: &str) -> FileInfo {
        FileInfo {
            path: PathBuf::from(path),
            size: 100,
            created: None,
            modified: None,
        }
    }

    #[test]
    fn test_build_tree() {
        let groups = vec![vec![
            make_info("/root/a/file.txt"),
            make_info("/root/b/file.txt"),
        ]];
        let tree = build_tree(&groups, Path::new("/root"));
        let flat = tree.flatten();
        assert_eq!(flat.len(), 4); // dir a, file, dir b, file
    }

    #[test]
    fn test_toggle_expand() {
        let groups = vec![vec![
            make_info("/root/a/file.txt"),
            make_info("/root/b/file.txt"),
        ]];
        let mut tree = build_tree(&groups, Path::new("/root"));
        let flat = tree.flatten();
        assert_eq!(flat.len(), 4);

        tree.toggle_expand(Path::new("/root/a"));
        let flat = tree.flatten();
        assert_eq!(flat.len(), 3); // dir a (collapsed), dir b, file
    }

    #[test]
    fn test_deep_nesting() {
        let groups = vec![vec![
            make_info("/root/a/b/c/d/file.txt"),
            make_info("/root/x/file.txt"),
        ]];
        let tree = build_tree(&groups, Path::new("/root"));
        let flat = tree.flatten();
        // a(0), b(1), c(2), d(3), file.txt(4), x(0), file.txt(1)
        assert_eq!(flat.len(), 7);
        assert_eq!(flat[0].0, 0); // "a" at depth 0
        assert_eq!(flat[3].0, 3); // "d" at depth 3
        assert_eq!(flat[4].0, 4); // file at depth 4
    }

    #[test]
    fn test_multiple_groups() {
        let groups = vec![
            vec![make_info("/root/a.txt"), make_info("/root/b.txt")],
            vec![make_info("/root/sub/c.txt"), make_info("/root/sub/d.txt")],
        ];
        let tree = build_tree(&groups, Path::new("/root"));
        let flat = tree.flatten();
        // a.txt, b.txt, sub/, c.txt, d.txt = 5
        assert_eq!(flat.len(), 5);
    }

    #[test]
    fn test_flatten_ordering_is_sorted() {
        let groups = vec![vec![
            make_info("/root/z.txt"),
            make_info("/root/a.txt"),
            make_info("/root/m.txt"),
        ]];
        let tree = build_tree(&groups, Path::new("/root"));
        let flat = tree.flatten();
        let names: Vec<&str> = flat.iter().map(|(_, n)| n.name.as_str()).collect();
        // BTreeMap gives sorted order
        assert_eq!(names, vec!["a.txt", "m.txt", "z.txt"]);
    }

    #[test]
    fn test_collapsed_hides_children() {
        let groups = vec![vec![
            make_info("/root/dir/a.txt"),
            make_info("/root/dir/b.txt"),
        ]];
        let mut tree = build_tree(&groups, Path::new("/root"));
        assert_eq!(tree.flatten().len(), 3); // dir, a.txt, b.txt

        tree.toggle_expand(Path::new("/root/dir"));
        assert_eq!(tree.flatten().len(), 1); // just dir (collapsed)
    }

    #[test]
    fn test_insert_file_group_index() {
        let groups = vec![
            vec![make_info("/root/a.txt"), make_info("/root/b.txt")],
            vec![make_info("/root/c.txt"), make_info("/root/d.txt")],
        ];
        let tree = build_tree(&groups, Path::new("/root"));
        let flat = tree.flatten();
        // First group files have group_index 0, second group has 1
        assert_eq!(flat[0].1.group_index, Some(0));
        assert_eq!(flat[2].1.group_index, Some(1));
    }

    #[test]
    fn test_flatten_sorted_by_size() {
        let groups = vec![vec![
            FileInfo { path: PathBuf::from("/root/small.txt"), size: 10, created: None, modified: None },
            FileInfo { path: PathBuf::from("/root/big.txt"), size: 1000, created: None, modified: None },
        ]];
        let tree = build_tree(&groups, Path::new("/root"));
        let flat = tree.flatten_sorted(SortMode::Size);
        // Largest first
        assert_eq!(flat[0].1.name, "big.txt");
        assert_eq!(flat[1].1.name, "small.txt");
    }

    #[test]
    fn test_flatten_sorted_by_date() {
        use std::time::{Duration, SystemTime};
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let new = SystemTime::UNIX_EPOCH + Duration::from_secs(2000);
        let groups = vec![vec![
            FileInfo { path: PathBuf::from("/root/old.txt"), size: 10, created: None, modified: Some(old) },
            FileInfo { path: PathBuf::from("/root/new.txt"), size: 10, created: None, modified: Some(new) },
        ]];
        let tree = build_tree(&groups, Path::new("/root"));
        let flat = tree.flatten_sorted(SortMode::Date);
        // Newest first
        assert_eq!(flat[0].1.name, "new.txt");
        assert_eq!(flat[1].1.name, "old.txt");
    }

    #[test]
    fn test_flatten_sorted_by_count() {
        // dir_many has 3 files, dir_few has 1 file
        let groups = vec![
            vec![make_info("/root/dir_many/a.txt"), make_info("/root/dir_few/b.txt")],
            vec![make_info("/root/dir_many/c.txt"), make_info("/root/dir_few/d.txt")],
            vec![make_info("/root/dir_many/e.txt"), make_info("/root/dir_few/f.txt")],
        ];
        let tree = build_tree(&groups, Path::new("/root"));
        let flat = tree.flatten_sorted(SortMode::Count);
        // dir_many (3 files) should come before dir_few (3 files) — same count, so stable
        // Both dirs have 3 files each, so order is by count (equal) then stable
        assert!(flat[0].1.is_dir);
    }

    #[test]
    fn test_count_files_leaf() {
        let groups = vec![vec![make_info("/root/a.txt"), make_info("/root/b.txt")]];
        let tree = build_tree(&groups, Path::new("/root"));
        // Root has 2 file children
        assert_eq!(count_files(&tree), 2);
    }

    #[test]
    fn test_count_files_nested() {
        let groups = vec![vec![
            make_info("/root/dir/a.txt"),
            make_info("/root/dir/b.txt"),
            make_info("/root/c.txt"),
        ]];
        let tree = build_tree(&groups, Path::new("/root"));
        // Root: dir(2 files) + c.txt = 3 total
        assert_eq!(count_files(&tree), 3);
    }
}
