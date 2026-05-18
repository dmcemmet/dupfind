use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::scanner::FileInfo;

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
        let mut result = Vec::new();
        for child in self.children.values() {
            Self::flatten_recursive(child, 0, &mut result);
        }
        result
    }

    fn flatten_recursive<'a>(
        node: &'a TreeNode,
        depth: usize,
        result: &mut Vec<(usize, &'a TreeNode)>,
    ) {
        result.push((depth, node));
        if node.is_dir && node.expanded {
            for child in node.children.values() {
                Self::flatten_recursive(child, depth + 1, result);
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
}
