use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use chrono::{DateTime, Local};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseEventKind, EnableMouseCapture, DisableMouseCapture};
use image::DynamicImage;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap},
};
use ratatui_image::{Image, picker::Picker};

use crate::preview;
use crate::scanner::{DuplicateGroups, FileInfo};
use crate::tree::{SortMode, TreeNode, build_tree, build_tree_indexed, count_files, dir_total_size};

#[derive(Clone, Copy, PartialEq)]
enum DialogButton {
    Cancel,
    Trash,
    DeletePermanently,
}

enum DialogState {
    None,
    ConfirmDelete {
        scroll: ListState,
        button: DialogButton,
    },
    Deleting {
        items: Vec<(PathBuf, DeleteStatus)>,
        current: usize,
        permanent: bool,
        scroll: ListState,
    },
    DeleteError {
        items: Vec<(PathBuf, DeleteStatus)>,
        current: usize,
        permanent: bool,
        error: String,
        button: DeleteErrorButton,
    },
    ConfirmQuit,
    Error(Vec<String>),
}

#[derive(Clone)]
#[allow(dead_code)]
enum DeleteStatus {
    Pending,
    Done,
    Failed(String),
    Skipped,
}

#[derive(Clone, Copy, PartialEq)]
enum DeleteErrorButton { Retry, Skip, Cancel }

pub struct App {
    groups: DuplicateGroups,
    root: PathBuf,
    tree: TreeNode,
    left_state: ListState,
    right_state: ListState,
    focus_right: bool,
    picker: Picker,
    read_only: bool,
    preview_cache: Option<(PathBuf, CachedPreview)>,
    async_preview: Arc<Mutex<Option<(PathBuf, CachedPreview)>>>,
    selected_for_delete: BTreeSet<PathBuf>,
    dialog: DialogState,
    sort_mode: SortMode,
    filter: String,
    filtering: bool,
    show_preview: bool,
    notification: Option<(String, Instant)>,
    terminal_height: u16,
}

enum CachedPreview {
    Text(String),
    Image(DynamicImage),
    Unsupported,
}

impl App {
    pub fn new(groups: DuplicateGroups, root: PathBuf, read_only: bool) -> Self {
        let mut sorted = groups;
        sort_groups(&mut sorted, SortMode::Name);
        let tree = build_tree(&sorted, &root);
        let mut left_state = ListState::default();
        if !tree.flatten().is_empty() {
            left_state.select(Some(0));
        }
        let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
        Self {
            groups: sorted,
            root,
            tree,
            left_state,
            right_state: ListState::default(),
            focus_right: false,
            picker,
            read_only,
            preview_cache: None,
            async_preview: Arc::new(Mutex::new(None)),
            selected_for_delete: BTreeSet::new(),
            dialog: DialogState::None,
            sort_mode: SortMode::Name,
            filter: String::new(),
            filtering: false,
            show_preview: true,
            notification: None,
            terminal_height: 24,
        }
    }

    pub fn run(&mut self, terminal: &mut ratatui::DefaultTerminal) -> std::io::Result<()> {
        crossterm::execute!(std::io::stderr(), EnableMouseCapture)?;
        let result = self.run_inner(terminal);
        crossterm::execute!(std::io::stderr(), DisableMouseCapture)?;
        result
    }

    fn run_inner(&mut self, terminal: &mut ratatui::DefaultTerminal) -> std::io::Result<()> {
        use std::time::Duration;
        loop {
            // Check for async preview result
            if let Ok(mut lock) = self.async_preview.try_lock() {
                if let Some(result) = lock.take() {
                    self.preview_cache = Some(result);
                }
            }

            // Process delete steps (one per frame to show progress)
            if let DialogState::Deleting { items, current, .. } = &self.dialog {
                if *current >= items.len() {
                    self.finish_delete();
                }
                // Don't process delete step here — do it after draw+poll
                // so the user sees the progress
            }

            terminal.draw(|f| {
                self.terminal_height = f.area().height;
                self.draw(f);
            })?;

            // Process one delete step after drawing (so user sees progress)
            if let DialogState::Deleting { items, current, .. } = &self.dialog {
                if *current < items.len() {
                    self.process_delete_step();
                    continue; // redraw immediately
                }
            }

            if !event::poll(Duration::from_millis(100))? {
                continue;
            }
            match event::read()? {
                Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if self.filtering {
                    match key.code {
                        KeyCode::Esc => {
                            self.filtering = false;
                            self.filter.clear();
                            self.rebuild_tree();
                        }
                        KeyCode::Enter => {
                            self.filtering = false;
                        }
                        KeyCode::Backspace => {
                            self.filter.pop();
                            self.rebuild_tree();
                        }
                        KeyCode::Char(c) => {
                            self.filter.push(c);
                            self.rebuild_tree();
                        }
                        _ => {}
                    }
                    continue;
                }
                match &self.dialog {
                    DialogState::None => match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            if !self.selected_for_delete.is_empty() {
                                self.dialog = DialogState::ConfirmQuit;
                            } else {
                                return Ok(());
                            }
                        }
                        KeyCode::Tab => self.focus_right = !self.focus_right,
                        KeyCode::Down | KeyCode::Char('j') => self.move_down(),
                        KeyCode::Up | KeyCode::Char('k') => self.move_up(),
                        KeyCode::Home => self.jump_top(),
                        KeyCode::End => self.jump_bottom(),
                        KeyCode::PageDown => self.page_down(),
                        KeyCode::PageUp => self.page_up(),
                        KeyCode::Enter => self.toggle(),
                        KeyCode::Char(' ') => self.toggle_select(),
                        KeyCode::Char('d') if !self.read_only => self.open_delete_dialog(),
                        KeyCode::Char('a') => self.select_all_in_group(),
                        KeyCode::Char('s') => self.cycle_sort(),
                        KeyCode::Char('p') => {
                            self.show_preview = !self.show_preview;
                            if !self.show_preview {
                                self.preview_cache = None;
                            }
                        }
                        KeyCode::Char('g') => self.goto_file_directory(),
                        KeyCode::Char('u') => { self.selected_for_delete.clear(); }
                        KeyCode::Char('x') => self.keep_one_select_rest(),
                        KeyCode::Char('i') => self.invert_selection(),
                        KeyCode::Char('c') => self.collapse_all(),
                        KeyCode::Char('e') => self.expand_all(),
                        KeyCode::Char('o') => self.open_externally(),
                        KeyCode::Char('E') => self.export_report(),
                        KeyCode::Char('/') => {
                            self.filtering = true;
                        }
                        _ => {}
                    },
                    DialogState::ConfirmDelete { .. } => match key.code {
                        KeyCode::Enter => {
                            if let DialogState::ConfirmDelete { button, .. } = &self.dialog {
                                match button {
                                    DialogButton::Trash => { self.execute_delete(false); }
                                    DialogButton::DeletePermanently => { self.execute_delete(true); }
                                    DialogButton::Cancel => { self.dialog = DialogState::None; }
                                }
                            }
                        }
                        KeyCode::Left => {
                            if let DialogState::ConfirmDelete { button, .. } = &mut self.dialog {
                                *button = match button {
                                    DialogButton::Cancel => DialogButton::DeletePermanently,
                                    DialogButton::Trash => DialogButton::Cancel,
                                    DialogButton::DeletePermanently => DialogButton::Trash,
                                };
                            }
                        }
                        KeyCode::Right | KeyCode::Tab => {
                            if let DialogState::ConfirmDelete { button, .. } = &mut self.dialog {
                                *button = match button {
                                    DialogButton::Cancel => DialogButton::Trash,
                                    DialogButton::Trash => DialogButton::DeletePermanently,
                                    DialogButton::DeletePermanently => DialogButton::Cancel,
                                };
                            }
                        }
                        KeyCode::Esc => {
                            self.dialog = DialogState::None;
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if let DialogState::ConfirmDelete { scroll, .. } = &mut self.dialog {
                                let len = self.selected_for_delete.len();
                                let i = scroll.selected().map_or(0, |i| (i + 1).min(len - 1));
                                scroll.select(Some(i));
                            }
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            if let DialogState::ConfirmDelete { scroll, .. } = &mut self.dialog {
                                let i = scroll.selected().map_or(0, |i| i.saturating_sub(1));
                                scroll.select(Some(i));
                            }
                        }
                        _ => {}
                    },
                    DialogState::Deleting { .. } => {
                        // Cancel deletion
                        if key.code == KeyCode::Esc {
                            if let DialogState::Deleting { items, current, .. } = &mut self.dialog {
                                for i in *current..items.len() {
                                    items[i].1 = DeleteStatus::Skipped;
                                }
                                *current = items.len();
                            }
                        }
                    },
                    DialogState::DeleteError { items: _, current: _, permanent: _, button: _, .. } => match key.code {
                        KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                            if let DialogState::DeleteError { button, .. } = &mut self.dialog {
                                *button = match button {
                                    DeleteErrorButton::Retry => DeleteErrorButton::Skip,
                                    DeleteErrorButton::Skip => DeleteErrorButton::Cancel,
                                    DeleteErrorButton::Cancel => DeleteErrorButton::Retry,
                                };
                            }
                        }
                        KeyCode::Enter => {
                            if let DialogState::DeleteError { items, current, permanent, button, .. } = &mut self.dialog {
                                match button {
                                    DeleteErrorButton::Retry => {
                                        // Go back to Deleting state to retry
                                        let items_c = items.clone();
                                        let cur = *current;
                                        let perm = *permanent;
                                        self.dialog = DialogState::Deleting {
                                            items: items_c, current: cur, permanent: perm,
                                            scroll: ListState::default().with_selected(Some(cur)),
                                        };
                                    }
                                    DeleteErrorButton::Skip => {
                                        items[*current].1 = DeleteStatus::Failed("Skipped".to_string());
                                        let items_c = items.clone();
                                        let cur = *current + 1;
                                        let perm = *permanent;
                                        let sel = cur.min(items_c.len().saturating_sub(1));
                                        self.dialog = DialogState::Deleting {
                                            items: items_c, current: cur, permanent: perm,
                                            scroll: ListState::default().with_selected(Some(sel)),
                                        };
                                    }
                                    DeleteErrorButton::Cancel => {
                                        for i in *current..items.len() {
                                            items[i].1 = DeleteStatus::Skipped;
                                        }
                                        let items_c = items.clone();
                                        let perm = *permanent;
                                        let len = items_c.len();
                                        self.dialog = DialogState::Deleting {
                                            items: items_c, current: len, permanent: perm,
                                            scroll: ListState::default(),
                                        };
                                    }
                                }
                            }
                        }
                        KeyCode::Esc => {
                            if let DialogState::DeleteError { items, current, permanent, .. } = &mut self.dialog {
                                for i in *current..items.len() {
                                    items[i].1 = DeleteStatus::Skipped;
                                }
                                let items_c = items.clone();
                                let perm = *permanent;
                                let len = items_c.len();
                                self.dialog = DialogState::Deleting {
                                    items: items_c, current: len, permanent: perm,
                                    scroll: ListState::default(),
                                };
                            }
                        }
                        _ => {}
                    },
                    DialogState::ConfirmQuit => match key.code {
                        KeyCode::Char('y') | KeyCode::Enter => return Ok(()),
                        _ => { self.dialog = DialogState::None; }
                    },
                    DialogState::Error(_) => {
                        self.dialog = DialogState::None;
                    }
                }
                }
                Event::Mouse(mouse) => {
                    match mouse.kind {
                        MouseEventKind::ScrollDown => self.move_down(),
                        MouseEventKind::ScrollUp => self.move_up(),
                        MouseEventKind::Down(_) => {
                            let x = mouse.column;
                            let total_w = crossterm::terminal::size().map(|(w,_)| w).unwrap_or(80);
                            let left_w = total_w * 40 / 100;
                            if x < left_w {
                                self.focus_right = false;
                            } else {
                                self.focus_right = true;
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    fn move_down(&mut self) {
        if self.focus_right {
            let len = self.right_items_len();
            if len > 0 {
                let i = self
                    .right_state
                    .selected()
                    .map_or(0, |i| (i + 1).min(len - 1));
                self.right_state.select(Some(i));
            }
        } else {
            let len = self.tree.flatten_sorted(self.sort_mode).len();
            if len > 0 {
                let i = self
                    .left_state
                    .selected()
                    .map_or(0, |i| (i + 1).min(len - 1));
                self.left_state.select(Some(i));
                self.right_state.select(Some(0));
                self.preview_cache = None;
            }
        }
    }

    fn move_up(&mut self) {
        if self.focus_right {
            let i = self
                .right_state
                .selected()
                .map_or(0, |i| i.saturating_sub(1));
            self.right_state.select(Some(i));
        } else {
            let i = self
                .left_state
                .selected()
                .map_or(0, |i| i.saturating_sub(1));
            self.left_state.select(Some(i));
            self.right_state.select(Some(0));
            self.preview_cache = None;
        }
    }

    fn jump_top(&mut self) {
        if self.focus_right {
            self.right_state.select(Some(0));
        } else {
            self.left_state.select(Some(0));
            self.right_state.select(Some(0));
            self.preview_cache = None;
        }
    }

    fn jump_bottom(&mut self) {
        if self.focus_right {
            let len = self.right_items_len();
            if len > 0 {
                self.right_state.select(Some(len - 1));
            }
        } else {
            let len = self.tree.flatten_sorted(self.sort_mode).len();
            if len > 0 {
                self.left_state.select(Some(len - 1));
                self.right_state.select(Some(0));
                self.preview_cache = None;
            }
        }
    }

    fn page_down(&mut self) {
        let page = (self.terminal_height.saturating_sub(12) as usize).max(5);
        if self.focus_right {
            let len = self.right_items_len();
            if len > 0 {
                let i = self.right_state.selected().map_or(0, |i| (i + page).min(len - 1));
                self.right_state.select(Some(i));
            }
        } else {
            let len = self.tree.flatten_sorted(self.sort_mode).len();
            if len > 0 {
                let i = self.left_state.selected().map_or(0, |i| (i + page).min(len - 1));
                self.left_state.select(Some(i));
                self.right_state.select(Some(0));
                self.preview_cache = None;
            }
        }
    }

    fn page_up(&mut self) {
        let page = (self.terminal_height.saturating_sub(12) as usize).max(5);
        if self.focus_right {
            let i = self.right_state.selected().map_or(0, |i| i.saturating_sub(page));
            self.right_state.select(Some(i));
        } else {
            let i = self.left_state.selected().map_or(0, |i| i.saturating_sub(page));
            self.left_state.select(Some(i));
            self.right_state.select(Some(0));
            self.preview_cache = None;
        }
    }

    fn keep_one_select_rest(&mut self) {
        // Keep the currently highlighted file in right pane, select all others in group
        let Some((sel, group_idx)) = self.selected_file() else { return };
        let right_idx = self.right_state.selected().unwrap_or(0);
        let keep_path = if self.focus_right && right_idx > 0 {
            let dupes: Vec<PathBuf> = self.groups[group_idx]
                .iter()
                .filter(|f| f.path != sel.path)
                .map(|f| f.path.clone())
                .collect();
            dupes.get(right_idx - 1).cloned().unwrap_or(sel.path.clone())
        } else {
            sel.path.clone()
        };
        for f in &self.groups[group_idx] {
            if f.path != keep_path {
                self.selected_for_delete.insert(f.path.clone());
            }
        }
        // Ensure the kept one is not selected
        self.selected_for_delete.remove(&keep_path);
    }

    fn invert_selection(&mut self) {
        if self.focus_right {
            // Invert in current group
            let Some((_, group_idx)) = self.selected_file() else { return };
            for f in &self.groups[group_idx] {
                if self.selected_for_delete.contains(&f.path) {
                    self.selected_for_delete.remove(&f.path);
                } else {
                    self.selected_for_delete.insert(f.path.clone());
                }
            }
        } else {
            // Invert in current directory
            let flat = self.tree.flatten_sorted(self.sort_mode);
            let Some(idx) = self.left_state.selected() else { return };
            let Some((_, node)) = flat.get(idx) else { return };
            let parent = if node.is_dir { node.path.clone() } else {
                node.path.parent().unwrap_or(&node.path).to_path_buf()
            };
            for (_, n) in &flat {
                if !n.is_dir {
                    if let Some(p) = n.path.parent() {
                        if p == parent {
                            if self.selected_for_delete.contains(&n.path) {
                                self.selected_for_delete.remove(&n.path);
                            } else {
                                self.selected_for_delete.insert(n.path.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    fn collapse_all(&mut self) {
        self.tree.set_all_expanded(false);
        self.left_state.select(Some(0));
        self.right_state.select(Some(0));
        self.preview_cache = None;
    }

    fn expand_all(&mut self) {
        self.tree.set_all_expanded(true);
        self.preview_cache = None;
    }

    fn open_externally(&mut self) {
        let path = if self.focus_right {
            let Some((sel, group_idx)) = self.selected_file() else { return };
            let right_idx = self.right_state.selected().unwrap_or(0);
            if right_idx == 0 {
                sel.path.clone()
            } else {
                let dupes: Vec<&FileInfo> = self.groups[group_idx]
                    .iter().filter(|f| f.path != sel.path).collect();
                match dupes.get(right_idx - 1) {
                    Some(f) => f.path.clone(),
                    None => return,
                }
            }
        } else {
            let flat = self.tree.flatten_sorted(self.sort_mode);
            let Some(idx) = self.left_state.selected() else { return };
            let Some((_, node)) = flat.get(idx) else { return };
            node.path.clone()
        };
        match std::process::Command::new("open").arg(&path).output() {
            Ok(output) if output.status.success() => {
                self.notify(format!("Opened: {}", path.file_name().unwrap_or_default().to_string_lossy()));
            }
            Ok(output) => {
                let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
                self.dialog = DialogState::Error(vec![format!("Failed to open: {err}")]);
            }
            Err(e) => {
                self.dialog = DialogState::Error(vec![format!("Failed to open: {e}")]);
            }
        }
    }

    fn export_report(&mut self) {
        let path = self.root.join("dupfinder_report.csv");
        let mut lines = vec!["Group,Size,Path".to_string()];
        for (i, group) in self.groups.iter().enumerate() {
            for f in group {
                let rel = f.path.strip_prefix(&self.root).unwrap_or(&f.path);
                lines.push(format!("{},{},{}", i + 1, f.size, rel.display()));
            }
        }
        let _ = std::fs::write(&path, lines.join("\n"));
        self.notify(format!("Exported to {}", path.display()));
    }

    fn notify(&mut self, msg: String) {
        self.notification = Some((msg, Instant::now()));
    }

    fn toggle(&mut self) {
        if self.focus_right {
            return;
        }
        let flat = self.tree.flatten_sorted(self.sort_mode);
        if let Some(idx) = self.left_state.selected()
            && let Some((_, node)) = flat.get(idx)
            && node.is_dir
        {
            let path = node.path.clone();
            self.tree.toggle_expand(&path);
        }
    }

    fn toggle_select(&mut self) {
        if !self.focus_right {
            // Select/deselect the file under cursor in left pane
            let flat = self.tree.flatten_sorted(self.sort_mode);
            if let Some(idx) = self.left_state.selected()
                && let Some((_, node)) = flat.get(idx)
                && !node.is_dir
            {
                let path = node.path.clone();
                if !self.selected_for_delete.remove(&path) {
                    self.selected_for_delete.insert(path);
                }
            } else {
                self.toggle();
            }
            return;
        }
        let Some((sel, group_idx)) = self.selected_file() else {
            return;
        };
        let right_idx = self.right_state.selected().unwrap_or(0);
        let path = if right_idx == 0 {
            sel.path.clone()
        } else {
            let dupes: Vec<PathBuf> = self.groups[group_idx]
                .iter()
                .filter(|f| f.path != sel.path)
                .map(|f| f.path.clone())
                .collect();
            match dupes.get(right_idx - 1) {
                Some(p) => p.clone(),
                None => return,
            }
        };
        if !self.selected_for_delete.remove(&path) {
            self.selected_for_delete.insert(path);
        }
    }

    fn select_all_in_group(&mut self) {
        if self.focus_right {
            // In right pane: select all copies except the primary
            let Some((sel, group_idx)) = self.selected_file() else {
                return;
            };
            let sel_path = sel.path.clone();
            for f in &self.groups[group_idx] {
                if f.path != sel_path {
                    self.selected_for_delete.insert(f.path.clone());
                }
            }
        } else {
            // In left pane: select all files in the current directory
            let flat = self.tree.flatten_sorted(self.sort_mode);
            let Some(idx) = self.left_state.selected() else {
                return;
            };
            let Some((_, node)) = flat.get(idx) else {
                return;
            };
            // Determine the parent directory
            let parent = if node.is_dir {
                node.path.clone()
            } else {
                node.path.parent().unwrap_or(&node.path).to_path_buf()
            };
            // Select all file nodes that are direct children of this directory
            for (_, n) in &flat {
                if !n.is_dir {
                    if let Some(p) = n.path.parent() {
                        if p == parent {
                            self.selected_for_delete.insert(n.path.clone());
                        }
                    }
                }
            }
        }
    }

    fn goto_file_directory(&mut self) {
        if !self.focus_right {
            return;
        }
        let Some((sel, group_idx)) = self.selected_file() else {
            return;
        };
        let right_idx = self.right_state.selected().unwrap_or(0);
        let target_path = if right_idx == 0 {
            sel.path.clone()
        } else {
            let dupes: Vec<&FileInfo> = self.groups[group_idx]
                .iter()
                .filter(|f| f.path != sel.path)
                .collect();
            match dupes.get(right_idx - 1) {
                Some(f) => f.path.clone(),
                None => return,
            }
        };
        // Find the target file in the tree and navigate to it
        let flat = self.tree.flatten_sorted(self.sort_mode);
        if let Some(pos) = flat.iter().position(|(_, n)| n.path == target_path) {
            self.left_state.select(Some(pos));
            self.right_state.select(Some(0));
            self.preview_cache = None;
            self.focus_right = false;
        } else if let Some(parent) = target_path.parent() {
            if let Some(_) = flat.iter().position(|(_, n)| n.path == parent) {
                // Parent exists but is collapsed — expand it
                self.tree.toggle_expand(parent);
                let flat = self.tree.flatten_sorted(self.sort_mode);
                let pos = flat.iter().position(|(_, n)| n.path == target_path)
                    .or_else(|| flat.iter().position(|(_, n)| n.path == parent))
                    .unwrap_or(0);
                self.left_state.select(Some(pos));
                self.right_state.select(Some(0));
                self.preview_cache = None;
                self.focus_right = false;
            }
        }
    }

    fn open_delete_dialog(&mut self) {
        if self.selected_for_delete.is_empty() {
            return;
        }
        self.dialog = DialogState::ConfirmDelete {
            scroll: ListState::default().with_selected(Some(0)),
            button: DialogButton::Cancel, // Cancel is default
        };
    }

    fn execute_delete(&mut self, permanent: bool) {
        let trash_dir = self.root.join(".dupfinder_trash");

        if !permanent {
            if let Err(e) = fs::create_dir_all(&trash_dir) {
                self.dialog = DialogState::Error(vec![format!("Cannot create trash dir: {e}")]);
                return;
            }
        }

        let items: Vec<(PathBuf, DeleteStatus)> = self.selected_for_delete.iter()
            .map(|p| (p.clone(), DeleteStatus::Pending))
            .collect();
        self.dialog = DialogState::Deleting {
            items,
            current: 0,
            permanent,
            scroll: ListState::default().with_selected(Some(0)),
        };
    }

    fn process_delete_step(&mut self) {
        let (path, permanent, _current) = {
            let DialogState::Deleting { items, current, permanent, .. } = &self.dialog else { return };
            if *current >= items.len() { return; }
            (items[*current].0.clone(), *permanent, *current)
        };

        let trash_dir = self.root.join(".dupfinder_trash");
        let result = if permanent {
            fs::remove_file(&path)
        } else if let Ok(rel) = path.strip_prefix(&self.root) {
            let dest = trash_dir.join(rel);
            if let Some(parent) = dest.parent() {
                let _ = fs::create_dir_all(parent);
            }
            fs::rename(&path, &dest).or_else(|_| fs::copy(&path, &dest).and_then(|_| fs::remove_file(&path)))
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::Other, "not under root"))
        };

        match result {
            Ok(_) => {
                if let DialogState::Deleting { items, current, scroll, .. } = &mut self.dialog {
                    items[*current].1 = DeleteStatus::Done;
                    *current += 1;
                    scroll.select(Some((*current).min(items.len().saturating_sub(1))));
                }
            }
            Err(e) => {
                // Transition to error dialog
                if let DialogState::Deleting { items, current, permanent, .. } = &mut self.dialog {
                    let err = e.to_string();
                    let items_clone = items.clone();
                    let cur = *current;
                    let perm = *permanent;
                    self.dialog = DialogState::DeleteError {
                        items: items_clone,
                        current: cur,
                        permanent: perm,
                        error: err,
                        button: DeleteErrorButton::Retry,
                    };
                }
            }
        }
    }

    fn finish_delete(&mut self) {
        let deleted: BTreeSet<PathBuf> = if let DialogState::Deleting { items, .. } = &self.dialog {
            items.iter()
                .filter(|(_, s)| matches!(s, DeleteStatus::Done))
                .map(|(p, _)| p.clone())
                .collect()
        } else { return };

        let count = deleted.len();
        for group in &mut self.groups {
            group.retain(|f| !deleted.contains(&f.path));
        }
        self.groups.retain(|g| g.len() > 1);
        self.selected_for_delete.clear();
        crate::cache::save(&self.root, &self.groups);
        self.rebuild_tree();
        self.preview_cache = None;
        self.dialog = DialogState::None;
        self.notify(format!("Deleted {count} file(s)"));
    }

    fn rebuild_tree(&mut self) {
        let filtered: Vec<(usize, &Vec<FileInfo>)> = if self.filter.is_empty() {
            self.groups.iter().enumerate().collect()
        } else {
            let f = self.filter.to_lowercase();
            self.groups
                .iter()
                .enumerate()
                .filter(|(_, g)| {
                    g.iter()
                        .any(|fi| fi.path.to_string_lossy().to_lowercase().contains(&f))
                })
                .collect()
        };
        self.tree = build_tree_indexed(&filtered, &self.root);
        let flat = self.tree.flatten_sorted(self.sort_mode);
        if flat.is_empty() {
            self.left_state.select(None);
            self.right_state.select(None);
        } else {
            let idx = self.left_state.selected().unwrap_or(0).min(flat.len() - 1);
            self.left_state.select(Some(idx));
            self.right_state.select(Some(0));
        }
        self.preview_cache = None;
        self.focus_right = false;
    }

    fn cycle_sort(&mut self) {
        self.sort_mode = match self.sort_mode {
            SortMode::Name => SortMode::Size,
            SortMode::Size => SortMode::Date,
            SortMode::Date => SortMode::Count,
            SortMode::Count => SortMode::Name,
        };
        sort_groups(&mut self.groups, self.sort_mode);
        self.rebuild_tree();
    }

    fn selected_file(&self) -> Option<(&FileInfo, usize)> {
        let flat = self.tree.flatten_sorted(self.sort_mode);
        let idx = self.left_state.selected()?;
        let (_, node) = flat.get(idx)?;
        let info = node.file_info.as_ref()?;
        let group_idx = node.group_index?;
        Some((info, group_idx))
    }

    fn right_items_len(&self) -> usize {
        self.selected_file()
            .map(|(sel, gi)| {
                1 + self.groups[gi]
                    .iter()
                    .filter(|f| f.path != sel.path)
                    .count()
            })
            .unwrap_or(0)
    }

    fn total_duplicate_count(&self) -> usize {
        self.groups.iter().map(|g| g.len()).sum::<usize>()
    }

    fn size_stats(&self) -> (u64, u64) {
        let mut total: u64 = 0;
        let mut wasted: u64 = 0;
        for group in &self.groups {
            let file_size = group[0].size;
            total += file_size * group.len() as u64;
            wasted += file_size * (group.len() as u64 - 1);
        }
        (total, wasted)
    }

    /// Check if all copies in any group are selected for deletion
    fn has_full_group_selected(&self) -> bool {
        self.groups
            .iter()
            .any(|g| g.iter().all(|f| self.selected_for_delete.contains(&f.path)))
    }

    fn ensure_preview(&mut self) {
        if !self.show_preview {
            return;
        }
        let Some((sel, _)) = self.selected_file() else {
            self.preview_cache = None;
            return;
        };
        let path = sel.path.clone();
        if self.preview_cache.as_ref().is_some_and(|(p, _)| *p == path) {
            return;
        }
        // Launch async preview load
        self.preview_cache = None;
        let result = Arc::clone(&self.async_preview);
        thread::spawn(move || {
            let cached = if preview::is_image(&path) {
                match preview::load_image_thumbnail(&path) {
                    Some(img) => CachedPreview::Image(img),
                    None => CachedPreview::Unsupported,
                }
            } else if preview::is_text(&path) {
                match preview::load_text_preview(&path) {
                    Some(text) => CachedPreview::Text(text),
                    None => CachedPreview::Unsupported,
                }
            } else {
                CachedPreview::Unsupported
            };
            if let Ok(mut lock) = result.lock() {
                *lock = Some((path, cached));
            }
        });
    }

    fn draw(&mut self, f: &mut Frame) {
        self.ensure_preview();

        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),
                Constraint::Length(6),
                Constraint::Length(2),
            ])
            .split(f.area());

        let main = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(outer[0]);

        let right_col = if self.show_preview {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(main[1])
        } else {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(100), Constraint::Length(0)])
                .split(main[1])
        };

        // Compute selected info as owned data to avoid borrow conflicts
        let selected_info: Option<(FileInfo, usize)> = self.selected_file()
            .map(|(info, idx)| (info.clone(), idx));

        self.draw_left_pane(f, main[0]);
        self.draw_right_pane(f, right_col[0], selected_info.as_ref());
        if self.show_preview {
            self.draw_preview(f, right_col[1]);
        }

        let details = self.build_details(selected_info.as_ref());
        let details_widget = Paragraph::new(details)
            .block(Block::default().borders(Borders::ALL).title(" Details "));
        f.render_widget(details_widget, outer[1]);

        self.draw_status_bar(f, outer[2]);
        self.draw_dialog(f);
    }

    fn draw_left_pane(&mut self, f: &mut Frame, area: Rect) {
        let left_width = area.width.saturating_sub(2) as usize;
        let flat = self.tree.flatten_sorted(self.sort_mode);
        let items: Vec<ListItem> = flat
            .iter()
            .map(|(depth, node)| {
                let indent = "  ".repeat(*depth);
                let icon = if node.is_dir {
                    if node.expanded { "▼ " } else { "▶ " }
                } else {
                    "  "
                };
                let marked = !node.is_dir && self.selected_for_delete.contains(&node.path);
                let prefix = if marked { "*" } else { " " };
                let style = if marked {
                    Style::default().fg(Color::Red)
                } else if node.is_dir {
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let size_str = if node.is_dir {
                    let count = count_files(node);
                    let size = dir_total_size(node);
                    format!(" [{}, {}]", count, format_size(size))
                } else if let Some(info) = &node.file_info {
                    format!(" [{}]", format_size(info.size))
                } else {
                    String::new()
                };
                let fixed_len = prefix.len() + indent.len() + icon.len() + size_str.len();
                let name = if !node.is_dir && fixed_len + node.name.len() > left_width {
                    let avail = left_width.saturating_sub(fixed_len);
                    truncate_middle(&node.name, avail)
                } else {
                    node.name.clone()
                };
                ListItem::new(Line::from(Span::styled(
                    format!("{prefix}{indent}{icon}{name}{size_str}"),
                    style,
                )))
            })
            .collect();

        let left_title = if self.filtering {
            format!(" Duplicates [/{}] ", self.filter)
        } else {
            " Duplicates ".to_string()
        };
        let border_style = if !self.focus_right {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(left_title)
                    .border_style(border_style),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );
        f.render_stateful_widget(list, area, &mut self.left_state);

        let mut scrollbar_state = ScrollbarState::new(flat.len())
            .position(self.left_state.selected().unwrap_or(0));
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area,
            &mut scrollbar_state,
        );
    }

    fn draw_right_pane(&mut self, f: &mut Frame, area: Rect, selected: Option<&(FileInfo, usize)>) {
        let (items, title) = if let Some((sel, group_idx)) = selected {
            let rel_sel = sel.path.strip_prefix(&self.root).unwrap_or(&sel.path);
            let marked_sel = self.selected_for_delete.contains(&sel.path);
            let prefix = if marked_sel { "* " } else { "" };
            let color = if marked_sel { Color::Red } else { Color::Yellow };
            let mut items = vec![ListItem::new(Line::from(vec![
                Span::styled(format!("{prefix}[<<] "), Style::default().fg(color)),
                Span::styled(format!("{}", rel_sel.display()), Style::default().fg(color)),
            ]))];
            items.extend(
                self.groups[*group_idx]
                    .iter()
                    .filter(|f| f.path != sel.path)
                    .map(|f| {
                        let rel = f.path.strip_prefix(&self.root).unwrap_or(&f.path);
                        let marked = self.selected_for_delete.contains(&f.path);
                        let prefix = if marked { "* " } else { "  " };
                        let style = if marked {
                            Style::default().fg(Color::Red)
                        } else {
                            Style::default()
                        };
                        ListItem::new(Line::from(Span::styled(
                            format!("{prefix}{}", rel.display()),
                            style,
                        )))
                    }),
            );
            (items, " Duplicate Locations ")
        } else {
            (vec![], " Duplicate Locations ")
        };

        let item_count = items.len();
        let border_style = if self.focus_right {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(border_style),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );
        f.render_stateful_widget(list, area, &mut self.right_state);

        let mut scrollbar_state = ScrollbarState::new(item_count)
            .position(self.right_state.selected().unwrap_or(0));
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area,
            &mut scrollbar_state,
        );
    }

    fn draw_preview(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::default().borders(Borders::ALL).title(" Preview ");
        match &self.preview_cache {
            Some((_, CachedPreview::Text(text))) => {
                let para = Paragraph::new(text.clone())
                    .block(block)
                    .wrap(Wrap { trim: false });
                f.render_widget(para, area);
            }
            Some((_, CachedPreview::Image(img))) => {
                let inner = block.inner(area);
                f.render_widget(block, area);
                if inner.width > 0
                    && inner.height > 0
                    && let Some(proto) =
                        preview::make_image_protocol(&mut self.picker, img, inner)
                {
                    f.render_widget(Image::new(&proto), inner);
                }
            }
            _ => {
                let para = Paragraph::new("No preview available")
                    .block(block)
                    .style(Style::default().fg(Color::DarkGray));
                f.render_widget(para, area);
            }
        }
    }

    fn draw_status_bar(&mut self, f: &mut Frame, area: Rect) {
        // Expire notification after 3 seconds
        if let Some((_, time)) = &self.notification {
            if time.elapsed().as_secs() >= 3 {
                self.notification = None;
            }
        }
        let sel_count = self.selected_for_delete.len();
        let dup_count = self.total_duplicate_count();
        let groups_count = self.groups.len();
        let (total_size, wasted_size) = self.size_stats();
        let sel_size: u64 = self.selected_for_delete.iter()
            .filter_map(|p| self.groups.iter().flatten().find(|f| &f.path == p))
            .map(|f| f.size)
            .sum();
        let sort_label = match self.sort_mode {
            SortMode::Name => "name",
            SortMode::Size => "size",
            SortMode::Date => "date",
            SortMode::Count => "count",
        };
        let ro_indicator = if self.read_only { " [RO]" } else { "" };
        let warn = if self.has_full_group_selected() {
            " ⚠ ALL COPIES SELECTED"
        } else {
            ""
        };

        // Line 1: stats
        let mut line1 = vec![
            Span::styled(
                format!(" {groups_count} groups, {dup_count} dupes"),
                Style::default().fg(Color::White),
            ),
            Span::raw("  "),
            Span::styled(
                format!("Total:{} Wasted:{}", format_size(total_size), format_size(wasted_size)),
                Style::default().fg(Color::Magenta),
            ),
            Span::raw("  "),
            Span::styled(
                if sel_count > 0 {
                    format!("{sel_count} sel ({})", format_size(sel_size))
                } else {
                    format!("{sel_count} sel")
                },
                if sel_count > 0 {
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
        ];
        if !warn.is_empty() {
            line1.push(Span::styled(warn, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
        }
        line1.push(Span::styled(ro_indicator, Style::default().fg(Color::Yellow)));
        line1.push(Span::styled(
            format!("  Sort:{sort_label}"),
            Style::default().fg(Color::DarkGray),
        ));

        // Line 2: context-aware keybind hints
        let hints = if self.focus_right {
            " [Space]Sel [x]KeepOne [a]SelAll [i]Invert [g]Goto [d]Del [o]Open [Tab]Pane [/]Filter [p]Preview [q]Quit"
        } else {
            " [Space]Sel [a]SelDir [x]KeepOne [i]Invert [u]Desel [c]Collapse [e]Expand [s]Sort [d]Del [o]Open [E]Export [Tab]Pane [/]Filter [p]Preview [q]Quit"
        };
        let line2 = vec![Span::styled(hints, Style::default().fg(Color::DarkGray))];

        // Override line1 with notification if active
        let line1_final = if let Some((msg, _)) = &self.notification {
            Line::from(Span::styled(format!(" ✓ {msg}"), Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)))
        } else {
            Line::from(line1)
        };
        let text = vec![line1_final, Line::from(line2)];
        f.render_widget(
            Paragraph::new(text).style(Style::default().bg(Color::Black)),
            area,
        );
    }

    fn draw_dialog(&mut self, f: &mut Frame) {
        let has_full_group = self.has_full_group_selected();
        let del_count = self.selected_for_delete.len();
        let file_lines: Vec<ListItem> = self
            .selected_for_delete
            .iter()
            .map(|p| {
                let rel = p.strip_prefix(&self.root).unwrap_or(p);
                ListItem::new(Line::from(Span::styled(
                    format!("  {}", rel.display()),
                    Style::default().fg(Color::Red),
                )))
            })
            .collect();

        match &mut self.dialog {
            DialogState::None => {}
            DialogState::ConfirmDelete { scroll, button } => {
                let area = centered_rect(60, 60, f.area());
                f.render_widget(Clear, area);

                let dialog_layout = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(3), Constraint::Length(3)])
                    .split(area);

                let warn_title = if has_full_group {
                    " ⚠ Move to trash? (ALL copies selected!) "
                } else {
                    " Move to trash? "
                };
                let list = List::new(file_lines)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(warn_title)
                            .border_style(Style::default().fg(Color::Red)),
                    )
                    .highlight_style(Style::default().bg(Color::DarkGray));
                f.render_stateful_widget(list, dialog_layout[0], scroll);

                let cancel_style = if *button == DialogButton::Cancel {
                    Style::default()
                        .bg(Color::White)
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                let trash_style = if *button == DialogButton::Trash {
                    Style::default()
                        .bg(Color::Yellow)
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Yellow)
                };
                let perm_style = if *button == DialogButton::DeletePermanently {
                    Style::default()
                        .bg(Color::Red)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Red)
                };
                let buttons = Line::from(vec![
                    Span::raw("  "),
                    Span::styled(" [ Cancel ] ", cancel_style),
                    Span::raw("  "),
                    Span::styled(" [ Move to Trash ] ", trash_style),
                    Span::raw("  "),
                    Span::styled(
                        format!(" [ Delete {} permanently ] ", del_count),
                        perm_style,
                    ),
                ]);
                let btn_block = Block::default().borders(Borders::ALL);
                let btn_para = Paragraph::new(buttons).block(btn_block);
                f.render_widget(btn_para, dialog_layout[1]);
            }
            DialogState::Error(errors) => {
                let area = centered_rect(60, 40, f.area());
                f.render_widget(Clear, area);
                let text: Vec<Line> = errors
                    .iter()
                    .map(|e| Line::from(Span::styled(e.clone(), Style::default().fg(Color::Red))))
                    .collect();
                let para = Paragraph::new(text)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" Errors (press any key) ")
                            .border_style(Style::default().fg(Color::Red)),
                    )
                    .wrap(Wrap { trim: false });
                f.render_widget(para, area);
            }
            DialogState::Deleting { items, current, scroll, .. } => {
                let area = centered_rect(60, 50, f.area());
                f.render_widget(Clear, area);
                let layout = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(3), Constraint::Length(3)])
                    .split(area);

                let file_items: Vec<ListItem> = items.iter().enumerate().map(|(i, (p, status))| {
                    let rel = p.strip_prefix(&self.root).unwrap_or(p);
                    let (icon, style) = match status {
                        DeleteStatus::Pending => ("○", Style::default().fg(Color::DarkGray)),
                        DeleteStatus::Done => ("✓", Style::default().fg(Color::Green)),
                        DeleteStatus::Failed(_) => ("✗", Style::default().fg(Color::Red)),
                        DeleteStatus::Skipped => ("–", Style::default().fg(Color::Yellow)),
                    };
                    let icon = if i == *current && *current < items.len() { "►" } else { icon };
                    ListItem::new(Line::from(Span::styled(format!("{icon} {}", rel.display()), style)))
                }).collect();

                let done = *current >= items.len();
                let title = if done { " Delete complete " } else { " Deleting... (ESC to cancel) " };
                let list = List::new(file_items)
                    .block(Block::default().borders(Borders::ALL).title(title).border_style(Style::default().fg(Color::Red)))
                    .highlight_style(Style::default().add_modifier(Modifier::BOLD));
                f.render_stateful_widget(list, layout[0], scroll);

                let ratio = if items.is_empty() { 1.0 } else { *current as f64 / items.len() as f64 };
                let gauge = Gauge::default()
                    .block(Block::default().borders(Borders::ALL))
                    .gauge_style(Style::default().fg(Color::Red))
                    .ratio(ratio.min(1.0))
                    .label(format!("{}/{}", current, items.len()));
                f.render_widget(gauge, layout[1]);
            }
            DialogState::DeleteError { error, button, items, current, .. } => {
                let area = centered_rect(60, 30, f.area());
                f.render_widget(Clear, area);
                let layout = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(3), Constraint::Length(3)])
                    .split(area);

                let rel = items[*current].0.strip_prefix(&self.root).unwrap_or(&items[*current].0);
                let text = vec![
                    Line::from(""),
                    Line::from(Span::styled(format!("  File: {}", rel.display()), Style::default().fg(Color::White))),
                    Line::from(Span::styled(format!("  Error: {error}"), Style::default().fg(Color::Red))),
                    Line::from(""),
                ];
                let para = Paragraph::new(text)
                    .block(Block::default().borders(Borders::ALL).title(" Delete failed ").border_style(Style::default().fg(Color::Red)))
                    .wrap(Wrap { trim: false });
                f.render_widget(para, layout[0]);

                let retry_s = if *button == DeleteErrorButton::Retry { Style::default().bg(Color::White).fg(Color::Black).add_modifier(Modifier::BOLD) } else { Style::default().fg(Color::White) };
                let skip_s = if *button == DeleteErrorButton::Skip { Style::default().bg(Color::Yellow).fg(Color::Black).add_modifier(Modifier::BOLD) } else { Style::default().fg(Color::Yellow) };
                let cancel_s = if *button == DeleteErrorButton::Cancel { Style::default().bg(Color::Red).fg(Color::White).add_modifier(Modifier::BOLD) } else { Style::default().fg(Color::Red) };
                let buttons = Line::from(vec![
                    Span::raw("  "),
                    Span::styled(" [ Retry ] ", retry_s),
                    Span::raw("  "),
                    Span::styled(" [ Skip ] ", skip_s),
                    Span::raw("  "),
                    Span::styled(" [ Cancel ] ", cancel_s),
                ]);
                f.render_widget(Paragraph::new(buttons).block(Block::default().borders(Borders::ALL)), layout[1]);
            }
            DialogState::ConfirmQuit => {
                let area = centered_rect(50, 20, f.area());
                f.render_widget(Clear, area);
                let sel_count = self.selected_for_delete.len();
                let text = vec![
                    Line::from(""),
                    Line::from(Span::styled(
                        format!("  {sel_count} file(s) selected but not deleted."),
                        Style::default().fg(Color::Yellow),
                    )),
                    Line::from(""),
                    Line::from("  Quit anyway? [y]es / [any key] cancel"),
                ];
                let para = Paragraph::new(text).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Confirm Quit ")
                        .border_style(Style::default().fg(Color::Yellow)),
                );
                f.render_widget(para, area);
            }
        }
    }

    fn build_details(&self, selected: Option<&(FileInfo, usize)>) -> Vec<Line<'static>> {
        let Some((sel, group_idx)) = selected else {
            return vec![Line::from("Select a file to see details")];
        };
        let right_idx = self.right_state.selected().unwrap_or(0);
        let dupes: Vec<&FileInfo> = self.groups[*group_idx]
            .iter()
            .filter(|f| f.path != sel.path)
            .collect();

        let mut lines = vec![Line::from(vec![
            Span::styled("Selected: ", Style::default().fg(Color::Yellow)),
            Span::raw(sel.path.display().to_string()),
        ])];
        lines.push(format_meta(sel));

        if right_idx > 0 {
            if let Some(dup) = dupes.get(right_idx - 1) {
                lines.push(Line::from(vec![
                    Span::styled("Duplicate: ", Style::default().fg(Color::Yellow)),
                    Span::raw(dup.path.display().to_string()),
                ]));
                lines.push(format_meta(dup));
            }
        } else if let Some(dup) = dupes.first() {
            lines.push(Line::from(vec![
                Span::styled("Duplicate: ", Style::default().fg(Color::Yellow)),
                Span::raw(dup.path.display().to_string()),
            ]));
            lines.push(format_meta(dup));
        }
        lines
    }
}

fn sort_groups(groups: &mut DuplicateGroups, mode: SortMode) {
    match mode {
        SortMode::Name => groups.sort_by(|a, b| a[0].path.cmp(&b[0].path)),
        SortMode::Size => groups.sort_by_key(|g| std::cmp::Reverse(g[0].size)),
        SortMode::Date => groups.sort_by(|a, b| b[0].modified.cmp(&a[0].modified)),
        SortMode::Count => groups.sort_by_key(|g| std::cmp::Reverse(g.len())),
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(v[1])[1]
}

fn format_meta(info: &FileInfo) -> Line<'static> {
    let size = format_size(info.size);
    let meta = if info.created.is_none() || info.modified.is_none() {
        std::fs::metadata(&info.path).ok()
    } else {
        None
    };
    let created = info
        .created
        .or_else(|| meta.as_ref().and_then(|m| m.created().ok()))
        .map(|t| {
            DateTime::<Local>::from(t)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "N/A".into());
    let modified = info
        .modified
        .or_else(|| meta.as_ref().and_then(|m| m.modified().ok()))
        .map(|t| {
            DateTime::<Local>::from(t)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "N/A".into());
    Line::from(format!(
        "  Size: {size}  |  Created: {created}  |  Modified: {modified}"
    ))
}

fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    for unit in UNITS {
        if size < 1024.0 {
            return format!("{size:.1} {unit}");
        }
        size /= 1024.0;
    }
    format!("{size:.1} PB")
}

fn truncate_middle(s: &str, max_len: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_len || max_len < 5 {
        return s.to_string();
    }
    let avail = max_len - 1; // 1 for '…'
    let head = avail / 2;
    let tail = avail - head;
    let start: String = chars[..head].iter().collect();
    let end: String = chars[chars.len() - tail..].iter().collect();
    format!("{start}…{end}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::FileInfo;

    #[test]
    fn test_format_size_bytes() {
        assert_eq!(format_size(0), "0.0 B");
        assert_eq!(format_size(1), "1.0 B");
        assert_eq!(format_size(512), "512.0 B");
        assert_eq!(format_size(1023), "1023.0 B");
    }

    #[test]
    fn test_format_size_kb() {
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
    }

    #[test]
    fn test_format_size_mb() {
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
    }

    #[test]
    fn test_format_size_gb() {
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0 GB");
    }

    #[test]
    fn test_format_size_tb() {
        assert_eq!(format_size(1024u64 * 1024 * 1024 * 1024), "1.0 TB");
    }

    #[test]
    fn test_format_size_pb() {
        assert_eq!(format_size(1024u64 * 1024 * 1024 * 1024 * 1024), "1.0 PB");
    }

    fn make_info(path: &str, size: u64) -> FileInfo {
        FileInfo {
            path: PathBuf::from(path),
            size,
            created: None,
            modified: None,
        }
    }

    /// Test size_stats logic directly (same algorithm as App::size_stats)
    fn size_stats(groups: &DuplicateGroups) -> (u64, u64) {
        let mut total: u64 = 0;
        let mut wasted: u64 = 0;
        for group in groups {
            let file_size = group[0].size;
            total += file_size * group.len() as u64;
            wasted += file_size * (group.len() as u64 - 1);
        }
        (total, wasted)
    }

    #[test]
    fn test_size_stats_single_group() {
        let groups = vec![vec![make_info("a", 100), make_info("b", 100)]];
        let (total, wasted) = size_stats(&groups);
        assert_eq!(total, 200);
        assert_eq!(wasted, 100);
    }

    #[test]
    fn test_size_stats_multiple_groups() {
        let groups = vec![
            vec![
                make_info("a", 100),
                make_info("b", 100),
                make_info("c", 100),
            ],
            vec![make_info("d", 50), make_info("e", 50)],
        ];
        let (total, wasted) = size_stats(&groups);
        assert_eq!(total, 400); // 300 + 100
        assert_eq!(wasted, 250); // 200 + 50
    }

    #[test]
    fn test_size_stats_empty() {
        let groups: DuplicateGroups = vec![];
        let (total, wasted) = size_stats(&groups);
        assert_eq!(total, 0);
        assert_eq!(wasted, 0);
    }

    #[test]
    fn test_sort_groups_by_name() {
        let mut groups = vec![vec![make_info("z.txt", 10)], vec![make_info("a.txt", 20)]];
        sort_groups(&mut groups, SortMode::Name);
        assert_eq!(groups[0][0].path, PathBuf::from("a.txt"));
    }

    #[test]
    fn test_sort_groups_by_size() {
        let mut groups = vec![vec![make_info("small", 10)], vec![make_info("big", 1000)]];
        sort_groups(&mut groups, SortMode::Size);
        assert_eq!(groups[0][0].size, 1000); // largest first
    }

    #[test]
    fn test_sort_groups_by_count() {
        let mut groups = vec![
            vec![make_info("a", 10), make_info("b", 10)],
            vec![make_info("c", 10), make_info("d", 10), make_info("e", 10)],
        ];
        sort_groups(&mut groups, SortMode::Count);
        assert_eq!(groups[0].len(), 3); // most dupes first
    }

    #[test]
    fn test_truncate_middle_short() {
        assert_eq!(truncate_middle("hi.txt", 20), "hi.txt");
    }

    #[test]
    fn test_truncate_middle_exact() {
        assert_eq!(truncate_middle("12345", 5), "12345");
    }

    #[test]
    fn test_truncate_middle_truncates() {
        let result = truncate_middle("long_filename_here.txt", 10);
        assert_eq!(result.chars().count(), 10);
        assert!(result.contains('…'));
        // Preserves start and end
        assert!(result.starts_with("long"));
        assert!(result.ends_with(".txt"));
    }

    #[test]
    fn test_truncate_middle_unicode() {
        let name = "Układ_stron_książki.pdf";
        let result = truncate_middle(name, 12);
        assert_eq!(result.chars().count(), 12);
        assert!(result.contains('…'));
    }

    #[test]
    fn test_truncate_middle_too_small() {
        // max_len < 5 returns original
        assert_eq!(truncate_middle("abcdef", 4), "abcdef");
    }

    #[test]
    fn test_sort_groups_by_date() {
        use std::time::{Duration, SystemTime};
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        let new = SystemTime::UNIX_EPOCH + Duration::from_secs(2000);
        let mut groups = vec![
            vec![FileInfo {
                path: PathBuf::from("old.txt"),
                size: 10,
                created: None,
                modified: Some(old),
            }],
            vec![FileInfo {
                path: PathBuf::from("new.txt"),
                size: 10,
                created: None,
                modified: Some(new),
            }],
        ];
        sort_groups(&mut groups, SortMode::Date);
        assert_eq!(groups[0][0].path, PathBuf::from("new.txt")); // newest first
    }
}
