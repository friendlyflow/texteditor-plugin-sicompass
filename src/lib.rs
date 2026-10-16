//! The text editor: a file as a list of its lines.
//!
//! A sicompass plugin: a program sicompass starts, with the user's rights. It
//! declares the whole disk (`"filesystem": ["/"]` in `plugin.json`, shown at
//! install), and reads and writes it with `std::fs`. The plugin navigates a
//! filesystem tree rooted at its `textEditorPath` setting and, when the user
//! enters a file, parses its content into a FFON tree using the rules in
//! [`parse`]:
//!
//! - Lines separated by `\n` are siblings.
//! - A line ending with `:` becomes a section header (`FfonElement::Obj`).
//! - A `{ … }` block following a section header supplies its children.
//!
//! Every FFON element produced from a file carries a `<src=N>` annotation
//! that encodes the 0-based source-line index. This lets `commit_edit` map
//! edits back to exact lines on disk without a lossy round-trip through the
//! parser.
//!
//! Undo: a line insert or delete is a `ProviderOp` with the line in its
//! payload, and a file or folder deleted from the directory view carries its
//! snapshot (`sicompass_sdk::fs_snapshot`), trashed through the app's
//! `desktop::trash`.
//!
//! A `.ff` file is not text but a binary tree of nodes, each hashed over its
//! children and its content (a Merkle tree, see [`ff`]). It opens as that
//! tree, and every Enter writes the whole file again.
//!
//! `textEditorPath` defaults to `~`, which the app expands to the user's home
//! folder before the plugin sees it.

mod desktop;
pub mod ff;
pub mod localize;
mod parse;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use desktop::{Desktop, HostDesktop};
use sicompass_sdk::ffon::FfonElement;
use sicompass_sdk::fs_snapshot;
use sicompass_sdk::plugin::{Descriptor, Plugin, PollResult, ProviderOp, host};
use sicompass_sdk::tags;
use sicompass_sdk::timeline::FsSideEffect;
use std::path::{Path, PathBuf};

/// The `ProviderOp` commands this plugin records.
const OP_DELETE_LINE: &str = "texteditor.delete_line";
const OP_INSERT_LINES: &str = "texteditor.insert_lines";
const OP_DELETE: &str = "texteditor.delete";
/// A `.ff` node inserted or deleted, with its subtree in the payload.
const OP_FF_INSERT: &str = "texteditor.ff_insert";
const OP_FF_DELETE: &str = "texteditor.ff_delete";

/// The `ff_path` step into a level's `header` row, which is rendered, not a
/// node. Like any position no node has, the tree finds nothing there.
const FF_HEADER: usize = usize::MAX - 1;

// ---------------------------------------------------------------------------
// TextEditorProvider
// ---------------------------------------------------------------------------

pub struct TextEditorProvider {
    /// The root path shown when navigating to the editor.
    text_editor_path: String,
    /// Current filesystem position (rooted at `text_editor_path`).
    current_fs_path: PathBuf,
    /// Levels entered inside a `.ff` file (a text file has none).
    ffon_sub_path: Vec<String>,
    /// Set to `true` when `text_editor_path` changes so the app re-fetches the FFON.
    refresh_pending: bool,
    /// Combined path returned by `current_path()`.
    current_path_str: String,
    /// Raw source lines of the currently-open file (verbatim, with indentation).
    source_lines: Vec<String>,
    /// The current file's rows, one per line, with `<src=N>` annotations and
    /// `<input>` wrappers so every line is editable.
    cached_ffon: Vec<FfonElement>,
    /// Which file produced the current cache (avoids re-reading on every fetch).
    loaded_path: Option<PathBuf>,
    /// Whether the source file ended with a newline (restored on every flush).
    trailing_newline: bool,
    /// The open `.ff` file's tree, when the open file is one and it decoded.
    ff: Option<ff::FfDoc>,
    /// Where in `ff` the user is: child positions from the root, one per
    /// `ffon_sub_path` segment.
    ff_path: Vec<usize>,
    /// Unified-timeline emission queue, drained by the app after each action.
    /// Populated by `delete_item` (in-file line deletes and file/folder
    /// trashing) so deletions land on the undo timeline.
    pending_timeline_entries: Vec<ProviderOp>,
    /// The OS trash, through the app (a fake in the tests).
    desktop: Box<dyn Desktop>,
    /// Why the last create, rename, delete or save failed, for the app's
    /// header (`poll`).
    error: Option<String>,
}

impl TextEditorProvider {
    pub fn with_desktop(desktop: Box<dyn Desktop>) -> Self {
        // Until the setting arrives: the app expands the manifest's `~`.
        let text_editor_path = String::new();
        let current_fs_path = PathBuf::from(&text_editor_path);
        let current_path_str = text_editor_path.clone();
        TextEditorProvider {
            text_editor_path,
            current_fs_path,
            ffon_sub_path: Vec::new(),
            refresh_pending: false,
            current_path_str,
            source_lines: Vec::new(),
            cached_ffon: Vec::new(),
            loaded_path: None,
            trailing_newline: false,
            ff: None,
            ff_path: Vec::new(),
            pending_timeline_entries: Vec::new(),
            desktop,
            error: None,
        }
    }

    pub fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }

    pub fn at_root(&self) -> bool {
        self.current_fs_path == self.root_path() && self.ffon_sub_path.is_empty()
    }

    pub fn needs_refresh(&self) -> bool {
        self.refresh_pending
    }

    pub fn clear_needs_refresh(&mut self) {
        self.refresh_pending = false;
    }

    fn record(&mut self, command: &str, payload: FfonElement, label: String) {
        self.pending_timeline_entries.push(ProviderOp {
            command: command.to_owned(),
            payload: sicompass_sdk::plugin::encode_one(&payload),
            label,
        });
    }

    /// Reload the open file when the cached `source_lines` no longer correspond
    /// to `current_fs_path` — e.g. an undo runs after navigation cleared the
    /// cache. Without this, a delete-line undo would splice into an empty
    /// `source_lines` and flush a truncated file.
    fn ensure_file_loaded(&mut self) {
        if self.loaded_path.as_deref() != Some(self.current_fs_path.as_path()) {
            self.load_file();
        }
    }

    /// Queue an `insert_lines` timeline entry for `lines` spliced in at
    /// source-line index `clamp`. Shared by the in-file insert (`<srcins=N>`)
    /// and the empty-file first-write paths of `commit_edit`. The `<src=N>`
    /// payload prefix records the position; the rest is the inserted lines
    /// joined by `\n`, so undo can remove exactly them and redo splice back.
    fn push_insert_lines_entry(&mut self, clamp: usize, lines: &[String]) {
        let payload_text = format!("{}{}", tags::format_src(clamp), lines.join("\n"));
        let label = if lines.len() == 1 {
            format!("insert line {}", clamp + 1)
        } else {
            format!("insert {} lines at {}", lines.len(), clamp + 1)
        };
        self.record(OP_INSERT_LINES, FfonElement::new_str(payload_text), label);
    }

    fn sync_path_str(&mut self) {
        let base = self.current_fs_path.to_str().unwrap_or("/");
        self.current_path_str = if self.ffon_sub_path.is_empty() {
            base.to_string()
        } else {
            format!("{}/{}", base, self.ffon_sub_path.join("/"))
        };
    }

    /// `p` through every symlink along it (see `sicompass_sdk::fs_links`).
    /// Navigation keeps the path the user took; only the filesystem calls see
    /// this one.
    fn real(&self, p: &Path) -> PathBuf {
        self.desktop.resolve(p)
    }

    fn root_path(&self) -> PathBuf {
        PathBuf::from(&self.text_editor_path)
    }

    fn is_in_file_view(&self) -> bool {
        self.real(&self.current_fs_path).is_file() || !self.ffon_sub_path.is_empty()
    }

    fn list_directory(&self, dir: &Path) -> Vec<FfonElement> {
        // An entry that cannot be read, or vanished meanwhile, is left out.
        let dir = self.real(dir);
        let Ok(read) = std::fs::read_dir(&dir) else {
            return vec![];
        };
        let mut entries: Vec<(bool, String)> = read
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter_map(|name| {
                // The entry's own type: a link to a folder is not listed as
                // one, as before.
                let is_dir = std::fs::symlink_metadata(dir.join(&name)).ok()?.is_dir();
                Some((is_dir, name))
            })
            .collect();
        entries.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .reverse()
                .then(a.1.to_lowercase().cmp(&b.1.to_lowercase()))
        });
        entries
            .into_iter()
            .map(|(is_dir, name)| {
                let marker = if is_dir { "<dir>" } else { "<file>" };
                let tagged = format!("{marker}{}", tags::format_input(&name));
                FfonElement::new_obj(&tagged)
            })
            .collect()
    }

    /// Load (or reload) the file at `current_fs_path` into `source_lines` and
    /// build the annotated `cached_ffon`.  Returns `false` if the file cannot
    /// be read.
    fn load_file(&mut self) -> bool {
        if self.is_ff_file() {
            return self.load_ff();
        }
        let contents = match std::fs::read_to_string(self.real(&self.current_fs_path)) {
            Ok(s) => s,
            Err(_) => return false,
        };
        // Keep verbatim lines for write-back (indentation + blank lines preserved).
        self.trailing_newline = contents.ends_with('\n');
        self.source_lines = contents.lines().map(str::to_owned).collect();

        // One annotated row per line, each wrapped in <input> so the app
        // renders it as editable.
        self.cached_ffon = wrap_ffon_in_input(parse::parse_lines(&contents));
        self.loaded_path = Some(self.current_fs_path.clone());
        true
    }

    fn fetch_file_content(&mut self) -> Vec<FfonElement> {
        // Load or serve from cache.
        if self.loaded_path.as_deref() != Some(self.current_fs_path.as_path()) && !self.load_file()
        {
            return vec![FfonElement::new_str("(binary or unreadable file)")];
        }
        if self.is_ff_file() {
            // A file that did not decode keeps its one explaining row.
            return match &self.ff {
                Some(_) => self.ff_level(),
                None => self.cached_ffon.clone(),
            };
        }
        self.cached_ffon.clone()
    }

    /// Write `source_lines` back to `current_fs_path` and rebuild `cached_ffon`.
    ///
    /// If a single `source_lines` entry contains embedded newlines — which
    /// happens when the user types a multi-line replacement or inserts text
    /// containing `\n` (Ctrl+Enter while in insert mode) — the joined output
    /// still produces the correct file on disk, but the in-memory vector
    /// would be out of sync with the file's line count, breaking any
    /// subsequent edit that addresses lines by index. Rebuild
    /// `source_lines` from the just-written content so it matches the file.
    fn flush_source_lines(&mut self) -> bool {
        match self.write_source_lines() {
            Ok(()) => true,
            Err(e) => {
                let name = self.current_fs_path.display().to_string();
                self.error = Some(failed("texteditor-error-save", &name, &io_reason(&e)));
                false
            }
        }
    }

    /// [`Self::flush_source_lines`], with the system's error when the file
    /// cannot be written (a file that needs sudo).
    fn write_source_lines(&mut self) -> std::io::Result<()> {
        let mut content = self.source_lines.join("\n");
        if self.trailing_newline {
            content.push('\n');
        }
        std::fs::write(self.real(&self.current_fs_path), &content)?;
        // Re-derive source_lines from the freshly-written content so each
        // vector slot maps to exactly one file line.
        self.source_lines = content.lines().map(str::to_owned).collect();
        // Rebuild cache from the just-written content.
        self.cached_ffon = wrap_ffon_in_input(parse::parse_lines(&content));
        Ok(())
    }

    fn rename_fs_item(&mut self, old: &str, new: &str) -> bool {
        let old_name = tags::strip_display(old);
        let new_name = tags::strip_display(new);
        if old_name.is_empty() || new_name.is_empty() || old_name == new_name {
            return false;
        }
        let dir = self.real(&self.current_fs_path);
        let old_name = old_name.trim_end_matches('/').trim_end_matches('\\');
        let new_name = new_name.trim_end_matches('/').trim_end_matches('\\');
        match std::fs::rename(dir.join(old_name), dir.join(new_name)) {
            Ok(()) => true,
            Err(e) => {
                let mut args = localize::Args::new();
                args.set("old", old_name);
                args.set("new", new_name);
                args.set("err", io_reason(&e));
                self.error = Some(localize::t_args("texteditor-error-rename", &args));
                false
            }
        }
    }
}

// ---------------------------------------------------------------------------
// FFON wrapping helper
// ---------------------------------------------------------------------------

/// Recursively wrap every Str and Obj key in `<input>...</input>` so the app
/// renders each element as editable.  The `<src=N>` annotation is preserved
/// inside the `<input>` wrapper so commit_edit can decode the source-line index.
fn wrap_ffon_in_input(elements: Vec<FfonElement>) -> Vec<FfonElement> {
    elements
        .into_iter()
        .map(|elem| match elem {
            FfonElement::Str(s) => FfonElement::new_str(tags::format_input(&s)),
            FfonElement::Obj(mut obj) => {
                obj.key = tags::format_input(&obj.key);
                obj.children = wrap_ffon_in_input(obj.children);
                FfonElement::Obj(obj)
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Provider trait implementation
// ---------------------------------------------------------------------------

impl Plugin for TextEditorProvider {
    fn new() -> Self {
        TextEditorProvider::with_desktop(Box::new(HostDesktop))
    }

    fn describe(&self) -> Descriptor {
        Descriptor {
            name: "texteditor".to_owned(),
            display_name: localize::t("texteditor-display-name"),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            stable_root_key: true,
            has_editor_semantics: true,
            path_is_filesystem: true,
            ..Default::default()
        }
    }

    /// Start where the user's `textEditorPath` setting says.
    fn init(&mut self) {
        if let Some(val) = host::get_setting("textEditorPath").filter(|s| !s.is_empty()) {
            self.text_editor_path = expand_home(&val);
        }
        self.current_fs_path = PathBuf::from(&self.text_editor_path);
        self.ffon_sub_path.clear();
        self.refresh_pending = false;
        self.loaded_path = None;
        self.source_lines.clear();
        self.cached_ffon.clear();
        self.ff = None;
        self.ff_path.clear();
        self.pending_timeline_entries.clear();
        self.sync_path_str();
    }

    fn poll(&mut self) -> PollResult {
        let needs_refresh = self.needs_refresh();
        self.clear_needs_refresh();
        PollResult {
            at_root: self.at_root(),
            error: self.take_error(),
            needs_refresh,
            structural_edit_here: true,
            dashboard_here: false,
            ..Default::default()
        }
    }

    fn fetch(&mut self) -> Vec<FfonElement> {
        if self.text_editor_path.trim().is_empty() {
            return vec![FfonElement::new_str("set text editor path in settings")];
        }
        if self.is_in_file_view() {
            self.fetch_file_content()
        } else {
            self.list_directory(&self.current_fs_path.clone())
        }
    }

    fn push_path(&mut self, segment: &str) {
        let clean = tags::strip_display(segment);
        let clean = clean.trim_end_matches('/');
        if self.real(&self.current_fs_path).is_file() {
            // A text file is one flat list of its lines: only a `.ff` file has
            // levels to enter. (A tab saved inside a code block, from when
            // braces were levels, reopens at the top of the file.)
            if self.is_ff_file() {
                let next = self.ff_child_path(segment, clean);
                self.ff_path = next;
                self.ffon_sub_path.push(clean.to_string());
            }
        } else {
            let candidate = self.current_fs_path.join(clean);
            if self.real(&candidate).exists() {
                // Entering a new path invalidates the file cache.
                self.loaded_path = None;
                self.source_lines.clear();
                self.cached_ffon.clear();
                self.ff = None;
                self.ff_path.clear();
                self.current_fs_path = candidate;
            }
        }
        self.sync_path_str();
    }

    fn pop_path(&mut self) {
        if !self.ffon_sub_path.is_empty() {
            self.ffon_sub_path.pop();
            self.ff_path.pop();
        } else if self.current_fs_path != self.root_path() {
            // Leaving a file — clear the cache.
            self.loaded_path = None;
            self.source_lines.clear();
            self.cached_ffon.clear();
            self.ff = None;
            self.ff_path.clear();
            self.current_fs_path.pop();
        }
        self.sync_path_str();
    }

    fn current_path(&self) -> &str {
        &self.current_path_str
    }

    fn set_current_path(&mut self, path: &str) {
        // A path can run past a file into the levels inside it
        // (`…/notes.ff/b:/d:`), which are not on disk. The app restores a tab
        // by setting its saved path a level at a time, and taking the whole of
        // it for a folder listed nothing there: the tab reopened at the top of
        // the file. Open the file, then step into each level as `Right` does.
        let mut file = PathBuf::from(path);
        let mut inside = Vec::new();
        while !self.real(&file).is_file() {
            let Some(name) = file.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                break;
            };
            inside.push(name);
            if !file.pop() {
                break;
            }
        }
        let in_file = !inside.is_empty() && self.real(&file).is_file();
        self.current_fs_path = if in_file { file } else { PathBuf::from(path) };
        self.ffon_sub_path.clear();
        self.loaded_path = None;
        self.source_lines.clear();
        self.cached_ffon.clear();
        self.ff = None;
        self.ff_path.clear();
        self.sync_path_str();
        if in_file {
            for segment in inside.iter().rev() {
                self.push_path(segment);
            }
        }
    }

    // ---- Editing -----------------------------------------------------------

    fn commit_edit(&mut self, old: &str, new: &str) -> bool {
        // Unwrap <input>...</input> wrappers that may be present (e.g. from
        // undo/redo paths where FsRename passes element keys verbatim).
        let old_inner = tags::extract_input(old).unwrap_or_else(|| old.to_owned());
        let new_inner = tags::extract_input(new).unwrap_or_else(|| new.to_owned());

        // A `.ff` file is a tree of nodes, not lines.
        if self.is_ff_file() {
            return self.ff_commit_edit(&old_inner, &new_inner);
        }

        // ── Case 1: in-file line REPLACE ────────────────────────────────────
        // old_inner starts with <src=N>: edit the Nth source line.
        if let Some((line_idx, _old_text)) = tags::extract_src(&old_inner) {
            if line_idx >= self.source_lines.len() {
                return false;
            }
            // new_inner may be:
            //  a) "<srcins=N>"  → undo of a prior line insert: delete line N
            //  b) plain text   → replace the line with new content
            if let Some(del_idx) = tags::extract_src_insert(&new_inner) {
                // Undo of a line insert: delete line del_idx.
                if del_idx < self.source_lines.len() {
                    self.source_lines.remove(del_idx);
                    return self.flush_source_lines();
                }
                return false;
            }

            // Normal line replace. The row shows the whole line, indentation
            // included, so the edited text is written as typed. Multi-line
            // replacements (input buffer contains `\n`, e.g. typed via
            // Ctrl+Enter) expand into multiple source lines.
            let new_text = tags::strip_display(&new_inner);
            let new_lines = split_lines(&new_text);
            if new_lines.len() == 1 && self.source_lines[line_idx] == new_lines[0] {
                return true; // nothing actually changed — still signal success
            }
            self.source_lines.splice(line_idx..line_idx + 1, new_lines);
            return self.flush_source_lines();
        }

        // ── Case 2: in-file line INSERT placeholder ─────────────────────────
        // old_inner is "<srcins=N>": insert a new line at position N, as
        // typed. Like Case 1, multi-line input expands into multiple lines;
        // empty input becomes a single blank line so pressing Ctrl+I and then
        // Enter inserts a true empty line.
        if let Some(insert_idx) = tags::extract_src_insert(&old_inner) {
            let new_text = tags::strip_display(&new_inner);
            let new_lines = split_lines(&new_text);
            let clamp = insert_idx.min(self.source_lines.len());
            self.source_lines
                .splice(clamp..clamp, new_lines.iter().cloned());
            if !self.flush_source_lines() {
                return false;
            }
            self.push_insert_lines_entry(clamp, &new_lines);
            return true;
        }

        // ── Case 2b: in-file I_PLACEHOLDER ──────────────────────────────────
        // An empty file (or empty FFON sub-section) gets an `i <input></input>`
        // placeholder seeded by `navigate_right_raw` so the user has something
        // to type into. The Enter handler then calls commit_edit with an empty
        // `old` string. Insert the typed text as new file lines.
        if old_inner.is_empty() && !new_inner.is_empty() && self.is_in_file_view() {
            let new_text = tags::strip_display(&new_inner);
            if new_text.is_empty() {
                return false;
            }
            let new_lines = split_lines(&new_text);
            let pos = self.source_lines.len();
            self.source_lines
                .splice(pos..pos, new_lines.iter().cloned());
            if !self.flush_source_lines() {
                return false;
            }
            // The first write into an empty file is a line insert, not a file
            // creation — record it as such so undo/redo restore the content.
            self.push_insert_lines_entry(pos, &new_lines);
            return true;
        }

        // ── Case 3: directory create (old is empty = new item from placeholder) ─
        if old_inner.is_empty() && !new_inner.is_empty() && !self.is_in_file_view() {
            return if new_inner.ends_with(':') {
                self.create_directory(new_inner.trim_end_matches(':'))
            } else {
                self.create_file(&new_inner)
            };
        }

        // ── Case 4: directory rename ─────────────────────────────────────────
        if !self.is_in_file_view() {
            return self.rename_fs_item(old, new);
        }

        false
    }

    // ---- File operations ---------------------------------------------------

    fn create_file(&mut self, name: &str) -> bool {
        if name.is_empty() || self.is_in_file_view() {
            return false;
        }
        // Never over a file of that name: `File::create` would empty it.
        match std::fs::File::create_new(self.real(&self.current_fs_path).join(name)) {
            Ok(_) => true,
            Err(e) => {
                self.error = Some(failed("texteditor-error-create-file", name, &io_reason(&e)));
                false
            }
        }
    }

    fn create_directory(&mut self, name: &str) -> bool {
        if name.is_empty() || self.is_in_file_view() {
            return false;
        }
        match std::fs::create_dir(self.real(&self.current_fs_path).join(name)) {
            Ok(()) => true,
            Err(e) => {
                self.error = Some(failed(
                    "texteditor-error-create-directory",
                    name,
                    &io_reason(&e),
                ));
                false
            }
        }
    }

    fn delete_item(&mut self, name: &str) -> bool {
        // In-file: delete the indicated source line.
        let inner = tags::extract_input(name).unwrap_or_else(|| name.to_owned());
        if self.is_ff_file() {
            return self.ff_delete(&inner);
        }
        if let Some((line_idx, _)) = tags::extract_src(&inner) {
            if line_idx >= self.source_lines.len() {
                return false;
            }
            // Capture the verbatim line (indentation included) so undo can
            // restore it exactly. The recorded `<src=N>` prefix encodes the
            // index; the rest is the raw line text.
            let verbatim = self.source_lines[line_idx].clone();
            self.source_lines.remove(line_idx);
            if !self.flush_source_lines() {
                return false;
            }
            let payload =
                FfonElement::new_str(format!("{}{}", tags::format_src(line_idx), verbatim,));
            self.record(
                OP_DELETE_LINE,
                payload,
                format!("delete line {}", line_idx + 1),
            );
            return true;
        }

        // Directory view: move file/folder to OS trash.
        let clean = tags::strip_display(name);
        let clean = clean.trim_end_matches('/').trim_end_matches('\\');
        if clean.is_empty() {
            return false;
        }
        let full = self.real(&self.current_fs_path).join(clean);
        // Snapshot before trashing so an undo can restore even if the OS trash
        // is later emptied (see `sicompass_sdk::fs_snapshot`).
        let side_effect = fs_snapshot::snapshot_for_delete(&full);
        if let Err(e) = self.desktop.trash(&full) {
            self.error = Some(failed("texteditor-error-delete", clean, &reason(&e)));
            return false;
        }
        let payload = FfonElement::new_str(B64.encode(fs_snapshot::encode(&side_effect)));
        self.record(OP_DELETE, payload, format!("delete {clean}"));
        true
    }

    /// A folder or file the user may not write to (one that needs sudo) says
    /// so before anything is typed, in the words a write there would get.
    fn cannot_add_here(&mut self) -> Option<String> {
        if self.text_editor_path.trim().is_empty() {
            return None;
        }
        let at = self.real(&self.current_fs_path);
        let (key, err) = if self.is_in_file_view() {
            ("texteditor-error-change-here", may_write(&at, false).err()?)
        } else {
            ("texteditor-error-add-here", may_write(&at, true).err()?)
        };
        let mut args = localize::Args::new();
        args.set("path", self.current_fs_path.display());
        args.set("err", io_reason(&err));
        Some(localize::t_args(key, &args))
    }

    fn take_timeline_entries(&mut self) -> Vec<ProviderOp> {
        std::mem::take(&mut self.pending_timeline_entries)
    }

    fn undo(&mut self, entry: &ProviderOp) -> Result<(), String> {
        let Some(payload) = sicompass_sdk::plugin::decode_one(&entry.payload) else {
            return Ok(());
        };
        match entry.command.as_str() {
            OP_DELETE => {
                let Some(side_effect) = decode_snapshot(&payload) else {
                    return Ok(());
                };
                let desktop = &self.desktop;
                fs_snapshot::restore(&side_effect, |p| desktop.restore(p))
            }
            OP_DELETE_LINE => {
                let Some((idx, verbatim)) = decode_line_payload(&payload) else {
                    return Ok(());
                };
                self.ensure_file_loaded();
                let clamp = idx.min(self.source_lines.len());
                self.source_lines.splice(clamp..clamp, [verbatim]);
                self.flush_or("texteditor-error-undo-delete-line-write-failed")
            }
            OP_INSERT_LINES => {
                // Undo a line insert: remove the inserted lines back off disk.
                let Some((idx, joined)) = decode_line_payload(&payload) else {
                    return Ok(());
                };
                let count = joined.split('\n').count();
                self.ensure_file_loaded();
                let start = idx.min(self.source_lines.len());
                let end = (start + count).min(self.source_lines.len());
                self.source_lines.drain(start..end);
                self.flush_or("texteditor-error-undo-insert-line-write-failed")
            }
            OP_FF_INSERT => self.ff_take_back(&payload, "texteditor-error-undo-ff-write-failed"),
            OP_FF_DELETE => self.ff_put_back(&payload, "texteditor-error-undo-ff-write-failed"),
            _ => Ok(()),
        }
    }

    fn redo(&mut self, entry: &ProviderOp) -> Result<(), String> {
        let Some(payload) = sicompass_sdk::plugin::decode_one(&entry.payload) else {
            return Ok(());
        };
        match entry.command.as_str() {
            OP_DELETE => {
                // Re-trash at the absolute original path recorded in the
                // snapshot; the cursor may have moved since the delete.
                let Some(side_effect) = decode_snapshot(&payload) else {
                    return Ok(());
                };
                let path: Option<&Path> = match &side_effect {
                    FsSideEffect::TrashedFile { original_path, .. }
                    | FsSideEffect::TrashedDir { original_path, .. } => Some(original_path),
                    FsSideEffect::RenameOnly { from, .. } => Some(from),
                    FsSideEffect::None => None,
                };
                match path {
                    Some(path) => self.desktop.trash(path).map_err(|e| {
                        let mut args = localize::Args::new();
                        args.set("err", e);
                        localize::t_args("texteditor-error-redo-delete-trash-failed", &args)
                    }),
                    None => Ok(()),
                }
            }
            OP_DELETE_LINE => {
                let Some((idx, _)) = decode_line_payload(&payload) else {
                    return Ok(());
                };
                self.ensure_file_loaded();
                if idx >= self.source_lines.len() {
                    return Ok(());
                }
                self.source_lines.remove(idx);
                self.flush_or("texteditor-error-redo-delete-line-write-failed")
            }
            OP_INSERT_LINES => {
                // Redo a line insert: splice the recorded lines back in.
                let Some((idx, joined)) = decode_line_payload(&payload) else {
                    return Ok(());
                };
                self.ensure_file_loaded();
                let lines: Vec<String> = joined.split('\n').map(str::to_owned).collect();
                let clamp = idx.min(self.source_lines.len());
                self.source_lines.splice(clamp..clamp, lines);
                self.flush_or("texteditor-error-redo-insert-line-write-failed")
            }
            OP_FF_INSERT => self.ff_put_back(&payload, "texteditor-error-redo-ff-write-failed"),
            OP_FF_DELETE => self.ff_take_back(&payload, "texteditor-error-redo-ff-write-failed"),
            _ => Ok(()),
        }
    }

    // ---- Commands ----------------------------------------------------------

    fn commands(&self) -> Vec<String> {
        if self.is_in_file_view() {
            vec![]
        } else {
            vec!["create directory".to_owned(), "create file".to_owned()]
        }
    }

    fn handle_command(
        &mut self,
        cmd: &str,
        _elem_key: &str,
        _elem_type: i32,
    ) -> Result<Option<FfonElement>, String> {
        if matches!(cmd, "create directory" | "create file")
            && let Some(why) = self.cannot_add_here()
        {
            return Err(why);
        }
        Ok(match cmd {
            "create directory" => Some(FfonElement::new_obj("<input></input>")),
            "create file" => Some(FfonElement::new_str("<input></input>".to_owned())),
            _ => None,
        })
    }

    // ---- Settings ----------------------------------------------------------

    fn on_setting_change(&mut self, key: &str, value: &str) {
        if key != "textEditorPath" {
            return;
        }
        let value = expand_home(value);
        if value != self.text_editor_path {
            self.current_fs_path = PathBuf::from(&value);
            self.text_editor_path = value;
            self.ffon_sub_path.clear();
            self.loaded_path = None;
            self.source_lines.clear();
            self.cached_ffon.clear();
            self.ff = None;
            self.ff_path.clear();
            self.refresh_pending = true;
            self.sync_path_str();
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

impl TextEditorProvider {
    /// Write the lines back, or say why not in the user's language.
    fn flush_or(&mut self, error_id: &str) -> Result<(), String> {
        self.write_source_lines().map_err(|_| localize::t(error_id))
    }
}

// ---------------------------------------------------------------------------
// `.ff` files
// ---------------------------------------------------------------------------

impl TextEditorProvider {
    /// Whether the open file is a `.ff` tree.
    fn is_ff_file(&self) -> bool {
        self.current_fs_path.extension().is_some_and(|e| e == "ff")
            && self.real(&self.current_fs_path).is_file()
    }

    /// Read and decode the `.ff` file at `current_fs_path`. One that does not
    /// decode opens as a single row saying why. One whose hashes do not match
    /// opens with a warning, and its next save writes them right.
    fn load_ff(&mut self) -> bool {
        let Ok(bytes) = std::fs::read(self.real(&self.current_fs_path)) else {
            return false;
        };
        self.loaded_path = Some(self.current_fs_path.clone());
        self.source_lines.clear();
        self.cached_ffon.clear();
        match ff::decode(&bytes) {
            Ok(root) => {
                if !root.verify() {
                    let mut args = localize::Args::new();
                    args.set("name", self.current_fs_path.display());
                    self.error = Some(localize::t_args("texteditor-error-ff-hash", &args));
                }
                self.ff = Some(ff::FfDoc::new(root));
            }
            Err(e) => {
                self.ff = None;
                let mut args = localize::Args::new();
                args.set("err", ff_reason(e));
                let row = localize::t_args("texteditor-ff-invalid", &args);
                self.cached_ffon = vec![FfonElement::new_str(row)];
            }
        }
        true
    }

    /// The path of the node `segment` names, one level below `ff_path`: by
    /// the `<src=N>` the app passes along, else the first child whose content
    /// is `clean`. A segment that names nothing still takes a level, so
    /// `pop_path` stays in step.
    fn ff_child_path(&mut self, segment: &str, clean: &str) -> Vec<usize> {
        self.ensure_file_loaded();
        let mut path = self.ff_path.clone();
        let Some(doc) = &self.ff else {
            path.push(usize::MAX);
            return path;
        };
        let inner = tags::extract_input(segment).unwrap_or_else(|| segment.to_owned());
        // The header row has no `<src=N>`: every node does.
        if tags::extract_src(&inner).is_none() && clean == localize::t("texteditor-ff-header") {
            path.push(FF_HEADER);
            return path;
        }
        if let Some(found) = tags::extract_src(&inner).and_then(|(n, _)| doc.path_of(n))
            && found.len() == self.ffon_sub_path.len() + 1
        {
            return found;
        }
        let pos = doc
            .node_at(&path)
            .and_then(|p| p.children.iter().position(|c| c.content == clean))
            .unwrap_or(usize::MAX);
        path.push(pos);
        path
    }

    /// The rows of the level at `ff_path`. Every level opens with a `header`
    /// row, the nested ones the app already holds included. Inside it is the
    /// hash of the node whose children the level shows: at the top, the root
    /// hash, which changes whenever anything in the file does.
    fn ff_level(&self) -> Vec<FfonElement> {
        let Some(doc) = &self.ff else {
            return Vec::new();
        };
        if let Some((&FF_HEADER, owner)) = self.ff_path.split_last() {
            let Some(hash) = doc.hash_hex(owner) else {
                return Vec::new();
            };
            let mut args = localize::Args::new();
            args.set("hash", hash);
            return vec![FfonElement::new_str(localize::t_args(
                "texteditor-ff-sha256",
                &args,
            ))];
        }
        with_headers(
            doc.to_ffon(&self.ff_path),
            &localize::t("texteditor-ff-header"),
        )
    }

    /// `commit_edit` in a `.ff` file. `old` and `new` are unwrapped from
    /// their `<input>`.
    fn ff_commit_edit(&mut self, old: &str, new: &str) -> bool {
        self.ensure_file_loaded();
        let Some(doc) = self.ff.as_mut() else {
            return false;
        };

        // Node N's content, or (an undo of an insert, `<srcins>` in `new`)
        // node N taken back out.
        if let Some((n, _)) = tags::extract_src(old) {
            if tags::extract_src_insert(new).is_some() {
                return doc.delete(n).is_some() && self.write_ff();
            }
            let text = tags::strip_display(new);
            return match doc.replace(n, &text) {
                Ok(true) => self.write_ff(),
                Ok(false) => true,
                Err(e) => self.ff_refuse(e),
            };
        }

        // A new node: at `<srcins=M>` (Ctrl+I, Ctrl+A), or after the last
        // child for the `i` placeholder of an empty level (an empty `old`).
        let text = tags::strip_display(new);
        let pos = match tags::extract_src_insert(old) {
            Some(m) => doc.srcins_position(&self.ff_path, m),
            None if old.is_empty() && !text.is_empty() => {
                doc.node_at(&self.ff_path).map(|p| p.children.len())
            }
            None => None,
        };
        let Some(pos) = pos else {
            return false;
        };
        let parent = self.ff_path.clone();
        let node = ff::Node::new(text);
        if let Err(e) = doc.insert(&parent, pos, node.clone()) {
            return self.ff_refuse(e);
        }
        if !self.write_ff() {
            return false;
        }
        self.record_ff(OP_FF_INSERT, &parent, pos, &node, "insert node");
        true
    }

    /// `delete_item` in a `.ff` file: node N with its subtree.
    fn ff_delete(&mut self, inner: &str) -> bool {
        self.ensure_file_loaded();
        let Some((n, _)) = tags::extract_src(inner) else {
            return false;
        };
        let Some((parent, pos, node)) = self.ff.as_mut().and_then(|doc| doc.delete(n)) else {
            return false;
        };
        if !self.write_ff() {
            return false;
        }
        self.record_ff(OP_FF_DELETE, &parent, pos, &node, "delete node");
        true
    }

    /// Say why the tree refused a change (a node's limits).
    fn ff_refuse(&mut self, e: ff::FfError) -> bool {
        self.error = Some(ff_reason(e));
        false
    }

    /// Queue an insert or delete of `node` at `pos` under `parent`, the
    /// subtree in the payload so both directions can be replayed.
    fn record_ff(
        &mut self,
        command: &str,
        parent: &[usize],
        pos: usize,
        node: &ff::Node,
        label: &str,
    ) {
        let Ok(bytes) = ff::encode(node) else {
            return;
        };
        let path: Vec<String> = parent.iter().map(usize::to_string).collect();
        let text = format!("{};{pos};{}", path.join("."), B64.encode(bytes));
        self.record(command, FfonElement::new_str(text), label.to_owned());
    }

    /// Undo an insert, or redo a delete: the recorded node comes back out.
    fn ff_take_back(&mut self, payload: &FfonElement, error_id: &str) -> Result<(), String> {
        let Some((parent, pos, _)) = decode_ff_payload(payload) else {
            return Ok(());
        };
        self.ensure_file_loaded();
        let Some(doc) = self.ff.as_mut() else {
            return Err(localize::t(error_id));
        };
        if doc.remove(&parent, pos).is_none() {
            return Ok(());
        }
        self.try_write_ff().map_err(|_| localize::t(error_id))
    }

    /// Undo a delete, or redo an insert: the recorded node goes back in.
    fn ff_put_back(&mut self, payload: &FfonElement, error_id: &str) -> Result<(), String> {
        let Some((parent, pos, node)) = decode_ff_payload(payload) else {
            return Ok(());
        };
        self.ensure_file_loaded();
        let Some(doc) = self.ff.as_mut() else {
            return Err(localize::t(error_id));
        };
        doc.insert(&parent, pos, node)
            .map_err(|_| localize::t(error_id))?;
        self.try_write_ff().map_err(|_| localize::t(error_id))
    }

    /// Write the tree to the open `.ff` file, or say why not.
    fn write_ff(&mut self) -> bool {
        match self.try_write_ff() {
            Ok(()) => true,
            Err(e) => {
                let name = self.current_fs_path.display().to_string();
                self.error = Some(failed("texteditor-error-save", &name, &io_reason(&e)));
                false
            }
        }
    }

    /// The bytes go to a file beside it first, which then replaces it, so a
    /// crash mid-write never leaves half a tree. Where no file can be made
    /// beside it (a folder the user may not write to), they go in place.
    fn try_write_ff(&self) -> std::io::Result<()> {
        let Some(doc) = &self.ff else {
            return Err(std::io::Error::other("no .ff file is open"));
        };
        let bytes = ff::encode(&doc.root).map_err(|e| std::io::Error::other(ff_reason(e)))?;
        let path = self.real(&self.current_fs_path);
        let mut tmp_name = std::ffi::OsString::from(".");
        tmp_name.push(path.file_name().unwrap_or_default());
        tmp_name.push(".tmp");
        let tmp = path.with_file_name(tmp_name);
        if std::fs::write(&tmp, &bytes).is_err() {
            return std::fs::write(&path, &bytes);
        }
        if let Ok(meta) = std::fs::metadata(&path) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        std::fs::rename(&tmp, &path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }
}

/// `elements`, and the children of every branch among them, each led by a
/// childless `header` Obj. Childless, so the app fetches it every time it is
/// opened and the hash it shows is never one from before the last edit.
fn with_headers(elements: Vec<FfonElement>, header: &str) -> Vec<FfonElement> {
    let mut out = Vec::with_capacity(elements.len() + 1);
    out.push(FfonElement::new_obj(header));
    out.extend(elements.into_iter().map(|elem| match elem {
        FfonElement::Obj(mut obj) => {
            obj.children = with_headers(std::mem::take(&mut obj.children), header);
            FfonElement::Obj(obj)
        }
        leaf => leaf,
    }));
    out
}

/// Why a `.ff` file or change was refused, in the user's language.
fn ff_reason(e: ff::FfError) -> String {
    localize::t(match e {
        ff::FfError::Truncated => "texteditor-ff-reason-truncated",
        ff::FfError::TrailingBytes => "texteditor-ff-reason-trailing",
        ff::FfError::InvalidUtf8 => "texteditor-ff-reason-utf8",
        ff::FfError::TooDeep => "texteditor-ff-reason-too-deep",
        ff::FfError::NoSuchNode => "texteditor-ff-reason-no-node",
        ff::FfError::TooManyChildren => "texteditor-error-ff-too-many",
        ff::FfError::ContentTooLong => "texteditor-error-ff-too-long",
    })
}

/// A `texteditor.ff_insert` / `texteditor.ff_delete` payload: the parent's
/// path (child positions joined by `.`), the position, and the subtree as
/// base64 `.ff` bytes, separated by `;`.
fn decode_ff_payload(payload: &FfonElement) -> Option<(Vec<usize>, usize, ff::Node)> {
    let mut parts = payload.as_str()?.splitn(3, ';');
    let path = parts.next()?;
    let parent = if path.is_empty() {
        Vec::new()
    } else {
        path.split('.')
            .map(|p| p.parse().ok())
            .collect::<Option<Vec<usize>>>()?
    };
    let pos = parts.next()?.parse().ok()?;
    let node = ff::decode(&B64.decode(parts.next()?).ok()?).ok()?;
    Some((parent, pos, node))
}

/// `key` filled in with the item's `name` and the reason it failed.
fn failed(key: &str, name: &str, why: &str) -> String {
    let mut args = localize::Args::new();
    args.set("name", name);
    args.set("err", why);
    localize::t_args(key, &args)
}

/// Why the system refused, short enough to read out: the user's language for
/// a missing right (a file or folder that needs sudo), else the system's own
/// words without their `(os error N)`.
fn io_reason(e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        return localize::t("texteditor-error-reason-permission-denied");
    }
    let text = e.to_string();
    match text.rfind(" (os error ") {
        Some(at) => text[..at].to_owned(),
        None => text,
    }
}

/// [`io_reason`] for an error that crossed the app as text (the trash): its
/// `(os error N)`, when it has one, says which error it was.
fn reason(text: &str) -> String {
    let code = text
        .strip_suffix(')')
        .and_then(|t| t.rsplit_once("(os error "))
        .and_then(|(_, n)| n.parse::<i32>().ok());
    match code {
        Some(n) => io_reason(&std::io::Error::from_raw_os_error(n)),
        None => text.to_owned(),
    }
}

/// Whether the user may write `path`: a file's contents, or (`dir`) new
/// entries in a folder, which also takes the right to search it. Judged by
/// the system as it will judge the write itself.
#[cfg(unix)]
fn may_write(path: &Path, dir: bool) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let mode = if dir {
        libc::W_OK | libc::X_OK
    } else {
        libc::W_OK
    };
    // SAFETY: a valid C string, which `access` only reads.
    if unsafe { libc::access(c.as_ptr(), mode) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Elsewhere the write itself is the only judge: it says why on Enter.
#[cfg(not(unix))]
fn may_write(_path: &Path, _dir: bool) -> std::io::Result<()> {
    Ok(())
}

/// `~` or `~/…` as the user's home folder; anything else as it is. The app
/// expands the setting before the plugin sees it, so this only matters for a
/// `~` that arrives some other way, which would otherwise name a folder `~`.
fn expand_home(value: &str) -> String {
    let rest = match value.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => rest,
        _ => return value.to_owned(),
    };
    match std::env::home_dir() {
        Some(home) if rest.is_empty() => home.to_string_lossy().into_owned(),
        Some(home) => home
            .join(rest.trim_start_matches(['/', '\\']))
            .to_string_lossy()
            .into_owned(),
        None => value.to_owned(),
    }
}

/// A directory-view delete's snapshot, from its payload (base64 text).
fn decode_snapshot(payload: &FfonElement) -> Option<FsSideEffect> {
    let bytes = B64.decode(payload.as_str()?).ok()?;
    fs_snapshot::decode(&bytes)
}

/// Decode a `texteditor.delete_line` / `texteditor.insert_lines` timeline
/// payload into its `(line_index, text)` pair. The payload key is `<src=N>text`,
/// optionally wrapped in `<input>…</input>`. For an insert the `text` is the
/// inserted lines joined by `\n`; for a delete it is the single removed line.
fn decode_line_payload(payload: &FfonElement) -> Option<(usize, String)> {
    let key = match payload {
        FfonElement::Str(s) => s.as_str(),
        FfonElement::Obj(o) => o.key.as_str(),
    };
    let inner = tags::extract_input(key).unwrap_or_else(|| key.to_owned());
    tags::extract_src(&inner).map(|(idx, text)| (idx, text.to_owned()))
}

/// Split `text` on `\n` into one source line per piece, each written as typed.
/// Empty input → a single empty line; trailing/leading `\n` produce blank
/// lines so the on-disk file actually contains them.
fn split_lines(text: &str) -> Vec<String> {
    text.split('\n').map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sicompass_sdk::placeholders::I_PLACEHOLDER;
    use tempfile::TempDir;

    fn make_tmp() -> TempDir {
        TempDir::new().expect("tempdir")
    }

    /// The OS trash, simulated: a trashed item moves into a folder of its own
    /// and can come back from it. Nothing a test deletes reaches the
    /// developer's real trash.
    #[derive(Clone, Default)]
    struct FakeDesktop(std::rc::Rc<std::cell::RefCell<Vec<(PathBuf, PathBuf, TempDir)>>>);

    impl Desktop for FakeDesktop {
        fn trash(&self, path: &Path) -> Result<(), String> {
            std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let dir = TempDir::new().unwrap();
            let kept = dir.path().join("item");
            std::fs::rename(path, &kept).map_err(|e| e.to_string())?;
            self.0.borrow_mut().push((path.to_path_buf(), kept, dir));
            Ok(())
        }

        fn restore(&self, path: &Path) -> Result<(), String> {
            let mut items = self.0.borrow_mut();
            let at = items
                .iter()
                .rposition(|(orig, _, _)| orig == path)
                .ok_or("no matching item in the trash")?;
            let (orig, kept, _dir) = items.remove(at);
            std::fs::rename(kept, orig).map_err(|e| e.to_string())
        }
    }

    fn editor() -> TextEditorProvider {
        TextEditorProvider::with_desktop(Box::new(FakeDesktop::default()))
    }

    fn make_text_editor(tmp: &TempDir) -> TextEditorProvider {
        let mut p = editor();
        p.on_setting_change("textEditorPath", tmp.path().to_str().unwrap());
        p
    }

    /// Outside sicompass there is no app to trash anything: the host desktop
    /// refuses, so a test can only ever reach the fake.
    #[test]
    fn outside_sicompass_the_host_desktop_touches_nothing() {
        let dir = make_tmp();
        let file = dir.path().join("x.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(HostDesktop.trash(&file).is_err());
        let mut p = TextEditorProvider::new();
        p.on_setting_change("textEditorPath", dir.path().to_str().unwrap());
        assert!(!p.delete_item("<input>x.txt</input>"));
        assert!(file.exists());
    }

    /// The fake must really take the item away, or every `!path.exists()`
    /// assertion here is inert; a path that is not there stays an error.
    #[test]
    fn the_fake_trash_takes_items_away() {
        let dir = make_tmp();
        let d = FakeDesktop::default();
        let file = dir.path().join("x.txt");
        std::fs::write(&file, b"x").unwrap();
        d.trash(&file).unwrap();
        assert!(!file.exists());
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("inner"), b"x").unwrap();
        d.trash(&sub).unwrap();
        assert!(!sub.exists());
        assert!(d.trash(&dir.path().join("never-existed")).is_err());
        d.restore(&file).unwrap();
        assert!(file.exists(), "and gives it back");
    }

    /// The directory-view delete's snapshot, out of its undo entry.
    fn snapshot_of(entry: &ProviderOp) -> FsSideEffect {
        assert_eq!(entry.command, OP_DELETE);
        decode_snapshot(&sicompass_sdk::plugin::decode_one(&entry.payload).unwrap()).unwrap()
    }

    // ---- basic fetch / navigation ------------------------------------------

    #[test]
    fn fetch_with_empty_path_returns_hint() {
        let mut p = TextEditorProvider {
            text_editor_path: String::new(),
            current_fs_path: PathBuf::new(),
            ffon_sub_path: vec![],
            refresh_pending: false,
            current_path_str: String::new(),
            source_lines: vec![],
            cached_ffon: vec![],
            loaded_path: None,
            trailing_newline: false,
            ff: None,
            ff_path: vec![],
            pending_timeline_entries: vec![],
            desktop: Box::new(FakeDesktop::default()),
            error: None,
        };
        let items = p.fetch();
        assert_eq!(items.len(), 1);
        assert!(items[0].as_str().unwrap().contains("set text editor path"));
    }

    #[test]
    fn on_setting_change_updates_text_editor_path() {
        let mut p = editor();
        p.on_setting_change("textEditorPath", "/tmp/test");
        assert_eq!(p.text_editor_path, "/tmp/test");
        assert_eq!(p.current_fs_path, PathBuf::from("/tmp/test"));
    }

    #[test]
    fn on_setting_change_sets_refresh_pending() {
        let mut p = editor();
        assert!(!p.needs_refresh());
        p.on_setting_change("textEditorPath", "/tmp/newpath");
        assert!(
            p.needs_refresh(),
            "changing the path must set refresh_pending"
        );
    }

    #[test]
    fn on_setting_change_same_path_does_not_set_refresh() {
        let mut p = editor();
        let same = p.text_editor_path.clone();
        p.on_setting_change("textEditorPath", &same);
        assert!(!p.needs_refresh(), "same path must not trigger a refresh");
    }

    #[test]
    fn clear_needs_refresh_clears_flag() {
        let mut p = editor();
        p.on_setting_change("textEditorPath", "/tmp/other");
        assert!(p.needs_refresh());
        p.clear_needs_refresh();
        assert!(!p.needs_refresh());
    }

    /// A `~` that reaches the plugin unexpanded is still the home folder, and
    /// only `~` or `~/…` are: `~user` and other paths are left as they are.
    #[test]
    fn a_tilde_path_is_the_home_folder() {
        let home = std::env::home_dir().expect("a home folder");
        let mut p = editor();
        p.on_setting_change("textEditorPath", "~");
        assert_eq!(PathBuf::from(&p.text_editor_path), home);
        p.on_setting_change("textEditorPath", "~/notes");
        assert_eq!(PathBuf::from(&p.text_editor_path), home.join("notes"));
        assert_eq!(expand_home("~someone/x"), "~someone/x");
        assert_eq!(expand_home("/srv/~"), "/srv/~");
    }

    #[test]
    fn on_setting_change_ignores_other_keys() {
        let mut p = editor();
        let original = p.text_editor_path.clone();
        p.on_setting_change("someOtherKey", "/etc");
        assert_eq!(p.text_editor_path, original);
        assert!(!p.needs_refresh());
    }

    #[test]
    fn a_text_file_has_no_levels_to_enter() {
        // Braces and `:` are plain lines, so Right on one enters nothing and
        // the path stays at the file.
        let tmp = make_tmp();
        let file = tmp.path().join("data.txt");
        std::fs::write(&file, "section:\n{\n  child\n}").unwrap();

        let mut p = make_text_editor(&tmp);
        p.push_path("data.txt");
        let path_at_file = p.current_path().to_string();

        p.push_path("section:");
        assert_eq!(p.current_path(), path_at_file);
        assert_eq!(p.fetch().len(), 4);

        // So Left leaves the file.
        p.pop_path();
        assert_eq!(p.current_path(), tmp.path().display().to_string());
    }

    #[test]
    fn push_pop_navigates_directories() {
        let tmp = make_tmp();
        std::fs::create_dir(tmp.path().join("inner")).unwrap();

        let mut p = make_text_editor(&tmp);
        p.push_path("inner");
        assert!(p.current_fs_path.ends_with("inner"));
        p.pop_path();
        assert_eq!(p.current_fs_path, PathBuf::from(tmp.path()));
    }

    #[test]
    fn pop_path_cannot_go_above_root() {
        let tmp = make_tmp();
        let mut p = make_text_editor(&tmp);
        p.pop_path();
        assert_eq!(p.current_fs_path, PathBuf::from(tmp.path()));
    }

    #[test]
    fn fetch_lists_directory_entries_with_input_wrappers() {
        let tmp = make_tmp();
        std::fs::write(tmp.path().join("hello.txt"), "hi").unwrap();
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();

        let mut p = make_text_editor(&tmp);
        let items = p.fetch();
        assert!(!items.is_empty());
        // All entries must be wrapped in <input>
        for item in &items {
            let key = match item {
                FfonElement::Obj(o) => o.key.as_str(),
                FfonElement::Str(s) => s.as_str(),
            };
            if key == I_PLACEHOLDER {
                continue;
            }
            assert!(
                tags::has_input(key),
                "directory entry '{}' must be wrapped in <input>",
                key
            );
        }
        // File and directory names must appear somewhere
        let all_text: Vec<String> = items
            .iter()
            .map(|e| match e {
                FfonElement::Obj(o) => o.key.clone(),
                FfonElement::Str(s) => s.clone(),
            })
            .collect();
        assert!(all_text.iter().any(|n| n.contains("hello.txt")));
        assert!(all_text.iter().any(|n| n.contains("subdir")));
    }

    #[test]
    fn list_directory_tags_dirs_and_files() {
        let tmp = make_tmp();
        std::fs::write(tmp.path().join("hello.txt"), "hi").unwrap();
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();

        let mut p = make_text_editor(&tmp);
        let items = p.fetch();

        let mut saw_dir = false;
        let mut saw_file = false;
        for item in &items {
            let key = match item {
                FfonElement::Obj(o) => o.key.as_str(),
                FfonElement::Str(s) => s.as_str(),
            };
            if key == I_PLACEHOLDER {
                continue;
            }
            if key.contains("subdir") {
                assert!(tags::has_dir(key), "subdir entry must carry <dir>: {key}");
                assert!(
                    !tags::has_file(key),
                    "subdir entry must not carry <file>: {key}"
                );
                saw_dir = true;
            }
            if key.contains("hello.txt") {
                assert!(
                    tags::has_file(key),
                    "hello.txt entry must carry <file>: {key}"
                );
                assert!(
                    !tags::has_dir(key),
                    "hello.txt entry must not carry <dir>: {key}"
                );
                saw_file = true;
            }
        }
        assert!(saw_dir && saw_file, "expected both dir and file entries");
    }

    #[test]
    fn fetch_file_returns_annotated_content() {
        let tmp = make_tmp();
        let file = tmp.path().join("code.txt");
        std::fs::write(&file, "section:\n{\n  line1\n}\nplain").unwrap();

        let mut p = make_text_editor(&tmp);
        p.push_path("code.txt");
        let items = p.fetch();
        // One row per line, braces included: nothing opens a level.
        assert_eq!(items.len(), 5);
        assert!(items.iter().all(FfonElement::is_str));
        // All items must be <input>-wrapped and carry <src=N>
        for item in &items {
            let key = match item {
                FfonElement::Obj(o) => o.key.as_str(),
                FfonElement::Str(s) => s.as_str(),
            };
            assert!(tags::has_input(key), "element must have <input> wrapper");
            let inner = tags::extract_input(key).unwrap();
            assert!(
                tags::extract_src(&inner).is_some(),
                "inner content must have <src=N>, got: {}",
                inner
            );
        }
    }

    /// The text each row shows, `<input>` and `<src=N>` removed.
    fn row_texts(items: &[FfonElement]) -> Vec<String> {
        items
            .iter()
            .map(|item| {
                let inner = tags::extract_input(item.as_str().unwrap()).unwrap();
                tags::extract_src(&inner).unwrap().1.to_owned()
            })
            .collect()
    }

    #[test]
    fn code_file_rows_keep_their_indentation() {
        let tmp = make_tmp();
        std::fs::write(tmp.path().join("main.rs"), "fn x() {\n    if y {\n\tz();\n    }\n}\n")
            .unwrap();
        std::fs::write(tmp.path().join("a.py"), "def f():\n    return 1\n").unwrap();

        let mut p = make_text_editor(&tmp);
        p.push_path("main.rs");
        assert_eq!(
            row_texts(&p.fetch()),
            ["fn x() {", "    if y {", "\tz();", "    }", "}"]
        );
        p.pop_path();
        p.push_path("a.py");
        assert_eq!(row_texts(&p.fetch()), ["def f():", "    return 1"]);
    }

    #[test]
    fn a_tab_saved_inside_a_code_block_reopens_at_the_top_of_the_file() {
        // Braces used to be levels, so a saved tab can name one.
        let tmp = make_tmp();
        std::fs::write(tmp.path().join("main.rs"), "fn x() {\n    y();\n}\n").unwrap();

        let mut p = make_text_editor(&tmp);
        let path = format!("{}/main.rs/fn x() {{", tmp.path().display());
        p.set_current_path(&path);
        assert_eq!(row_texts(&p.fetch()), ["fn x() {", "    y();", "}"]);
    }

    #[test]
    fn text_editor_provider_has_editor_semantics() {
        let p = editor();
        assert!(p.describe().has_editor_semantics);
    }

    // ---- directory create / delete / rename --------------------------------

    /// A folder the test user cannot write to, as `/etc` is for a user
    /// without sudo, holding `kept.txt` (itself read-only). `None` when the
    /// user can write there anyway (root), where the test has nothing to show.
    #[cfg(unix)]
    struct ReadOnly(TempDir);

    #[cfg(unix)]
    impl ReadOnly {
        fn new() -> Option<Self> {
            use std::os::unix::fs::PermissionsExt;
            let dir = make_tmp();
            let kept = dir.path().join("kept.txt");
            std::fs::write(&kept, "alpha\nbeta").unwrap();
            std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o444)).unwrap();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
            let ro = ReadOnly(dir);
            if std::fs::write(ro.0.path().join("probe"), b"").is_ok() {
                return None;
            }
            Some(ro)
        }
    }

    #[cfg(unix)]
    impl Drop for ReadOnly {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(self.0.path(), std::fs::Permissions::from_mode(0o755));
        }
    }

    /// Every change the system refuses says why, in words a screen reader can
    /// read out, and leaves the disk as it was.
    #[cfg(unix)]
    #[test]
    fn a_refused_change_in_a_read_only_folder_says_why() {
        let Some(ro) = ReadOnly::new() else { return };
        let mut p = make_text_editor(&ro.0);
        assert!(!p.create_file("new.txt"));
        assert_eq!(
            p.take_error().as_deref(),
            Some("could not create new.txt: permission denied")
        );
        assert!(!p.create_directory("sub"));
        assert_eq!(
            p.take_error().as_deref(),
            Some("could not create folder sub: permission denied")
        );
        assert!(!p.commit_edit("<input>kept.txt</input>", "<input>moved.txt</input>"));
        assert_eq!(
            p.take_error().as_deref(),
            Some("could not rename kept.txt to moved.txt: permission denied")
        );
        assert!(!p.delete_item("<input>kept.txt</input>"));
        assert_eq!(
            p.take_error().as_deref(),
            Some("could not delete kept.txt: permission denied")
        );
        assert!(ro.0.path().join("kept.txt").exists());
        assert!(p.take_timeline_entries().is_empty());
    }

    /// Saving a line of a file that needs sudo says why, and the file keeps
    /// what it had.
    #[cfg(unix)]
    #[test]
    fn a_refused_save_says_why() {
        let Some(ro) = ReadOnly::new() else { return };
        let mut p = make_text_editor(&ro.0);
        p.push_path("kept.txt");
        p.fetch();
        let old = format!("<input>{}beta</input>", tags::format_src(1));
        assert!(!p.commit_edit(&old, "BETA"));
        let file = ro.0.path().join("kept.txt");
        assert_eq!(
            p.take_error(),
            Some(format!(
                "could not save {}: permission denied",
                file.display()
            ))
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\nbeta");
    }

    /// Where nothing can be written, the plugin says so before anything is
    /// typed: in the folder (and its create commands), and in the file.
    #[cfg(unix)]
    #[test]
    fn a_read_only_folder_or_file_says_nothing_can_be_added() {
        let Some(ro) = ReadOnly::new() else { return };
        let mut p = make_text_editor(&ro.0);
        let folder = format!("cannot add to {}: permission denied", ro.0.path().display());
        assert_eq!(p.cannot_add_here().as_deref(), Some(folder.as_str()));
        for cmd in ["create file", "create directory"] {
            assert_eq!(p.handle_command(cmd, "", 0), Err(folder.clone()), "{cmd}");
        }
        p.push_path("kept.txt");
        let file = ro.0.path().join("kept.txt");
        assert_eq!(
            p.cannot_add_here(),
            Some(format!(
                "cannot change {}: permission denied",
                file.display()
            ))
        );

        let tmp = make_tmp();
        std::fs::write(tmp.path().join("open.txt"), "x").unwrap();
        let mut writable = make_text_editor(&tmp);
        assert_eq!(writable.cannot_add_here(), None);
        writable.push_path("open.txt");
        assert_eq!(writable.cannot_add_here(), None);
    }

    /// Creating a name that is taken leaves what is there alone.
    #[test]
    fn creating_an_existing_file_keeps_its_contents() {
        let tmp = make_tmp();
        std::fs::write(tmp.path().join("kept.txt"), "precious").unwrap();
        let mut p = make_text_editor(&tmp);
        assert!(!p.create_file("kept.txt"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("kept.txt")).unwrap(),
            "precious"
        );
        assert!(
            p.take_error()
                .unwrap()
                .starts_with("could not create kept.txt: ")
        );
    }

    #[test]
    fn create_file_creates_on_disk() {
        let tmp = make_tmp();
        let mut p = make_text_editor(&tmp);
        assert!(p.create_file("new.txt"));
        assert!(tmp.path().join("new.txt").exists());
    }

    #[test]
    fn create_directory_creates_on_disk() {
        let tmp = make_tmp();
        let mut p = make_text_editor(&tmp);
        assert!(p.create_directory("mydir"));
        assert!(tmp.path().join("mydir").is_dir());
    }

    #[test]
    fn create_file_rejects_empty_name() {
        let tmp = make_tmp();
        let mut p = make_text_editor(&tmp);
        assert!(!p.create_file(""));
    }

    #[test]
    fn create_directory_rejects_empty_name() {
        let tmp = make_tmp();
        let mut p = make_text_editor(&tmp);
        assert!(!p.create_directory(""));
    }

    #[test]
    fn delete_item_moves_file_to_trash() {
        let tmp = make_tmp();
        let file = tmp.path().join("bye.txt");
        std::fs::write(&file, "").unwrap();
        let mut p = make_text_editor(&tmp);
        assert!(p.delete_item("<input>bye.txt</input>"));
        assert!(!file.exists());
    }

    #[test]
    fn commit_edit_renames_file() {
        let tmp = make_tmp();
        let file = tmp.path().join("old.txt");
        std::fs::write(&file, "").unwrap();
        let mut p = make_text_editor(&tmp);
        assert!(p.commit_edit("<input>old.txt</input>", "<input>new.txt</input>"));
        assert!(!file.exists());
        assert!(tmp.path().join("new.txt").exists());
    }

    // ---- file content editing -----------------------------------------------

    #[test]
    fn commit_edit_replaces_line_on_disk() {
        let tmp = make_tmp();
        let file = tmp.path().join("sample.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma").unwrap();

        let mut p = make_text_editor(&tmp);
        p.push_path("sample.txt");
        p.fetch(); // prime cache

        // Simulate editing line 1 ("beta") via <input><src=1>beta</input>
        let old = format!("<input>{}beta</input>", tags::format_src(1));
        let ok = p.commit_edit(&old, "BETA");
        assert!(ok);

        let written = std::fs::read_to_string(&file).unwrap();
        assert_eq!(written, "alpha\nBETA\ngamma");
    }

    #[test]
    fn commit_edit_preserves_indentation() {
        let tmp = make_tmp();
        let file = tmp.path().join("py.py");
        std::fs::write(&file, "def foo():\n    pass\n").unwrap();

        let mut p = make_text_editor(&tmp);
        p.push_path("py.py");
        p.fetch();

        // Line 1 is "    pass": the row shows the indent, and the edit keeps it.
        let old = format!("<input>{}    pass</input>", tags::format_src(1));
        let ok = p.commit_edit(&old, "    return 1");
        assert!(ok);

        let written = std::fs::read_to_string(&file).unwrap();
        assert_eq!(written, "def foo():\n    return 1\n");
    }

    #[test]
    fn commit_edit_writes_a_changed_indent_as_typed() {
        let tmp = make_tmp();
        let file = tmp.path().join("py.py");
        std::fs::write(&file, "def foo():\n    pass\n").unwrap();

        let mut p = make_text_editor(&tmp);
        p.push_path("py.py");
        p.fetch();

        let old = format!("<input>{}    pass</input>", tags::format_src(1));
        assert!(p.commit_edit(&old, "  \treturn 1"));

        let written = std::fs::read_to_string(&file).unwrap();
        assert_eq!(written, "def foo():\n  \treturn 1\n");
    }

    #[test]
    fn commit_edit_insert_new_line() {
        let tmp = make_tmp();
        let file = tmp.path().join("sample.txt");
        std::fs::write(&file, "alpha\ngamma").unwrap();

        let mut p = make_text_editor(&tmp);
        p.push_path("sample.txt");
        p.fetch();

        // Insert before line 1 ("gamma")
        let old = format!("<input>{}</input>", tags::format_src_insert(1));
        let ok = p.commit_edit(&old, "beta");
        assert!(ok);

        let written = std::fs::read_to_string(&file).unwrap();
        assert_eq!(written, "alpha\nbeta\ngamma");
    }

    #[test]
    fn commit_edit_undo_of_insert_deletes_line() {
        let tmp = make_tmp();
        let file = tmp.path().join("sample.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma").unwrap();

        let mut p = make_text_editor(&tmp);
        p.push_path("sample.txt");
        p.fetch();

        // Undo of an insert at line 1: old = current line 1, new = srcins placeholder
        let old = format!("<input>{}beta</input>", tags::format_src(1));
        let new = format!("<input>{}</input>", tags::format_src_insert(1));
        let ok = p.commit_edit(&old, &new);
        assert!(ok);

        let written = std::fs::read_to_string(&file).unwrap();
        assert_eq!(written, "alpha\ngamma");
    }

    #[test]
    fn delete_item_removes_source_line() {
        let tmp = make_tmp();
        let file = tmp.path().join("sample.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma").unwrap();

        let mut p = make_text_editor(&tmp);
        p.push_path("sample.txt");
        p.fetch();

        // Delete line 1 ("beta")
        let name = format!("<input>{}beta</input>", tags::format_src(1));
        let ok = p.delete_item(&name);
        assert!(ok);

        let written = std::fs::read_to_string(&file).unwrap();
        assert_eq!(written, "alpha\ngamma");
    }

    #[test]
    fn commit_edit_multiline_replace_writes_each_line_verbatim() {
        // Multi-line input from Ctrl+Enter expands into multiple file lines,
        // each with the indentation typed for it.
        let tmp = make_tmp();
        let file = tmp.path().join("py.py");
        std::fs::write(&file, "def foo():\n    pass\n").unwrap();

        let mut p = make_text_editor(&tmp);
        p.push_path("py.py");
        p.fetch();

        // Replace line 1 ("    pass") with two lines.
        let old = format!("<input>{}    pass</input>", tags::format_src(1));
        let ok = p.commit_edit(&old, "    x = 1\n    y = 2");
        assert!(ok);

        let written = std::fs::read_to_string(&file).unwrap();
        assert_eq!(written, "def foo():\n    x = 1\n    y = 2\n");
    }

    #[test]
    fn commit_edit_after_multiline_replace_keeps_subsequent_edits_aligned() {
        // Regression: previously source_lines wasn't rebuilt after a
        // multi-line replace, so a single source slot ended up containing
        // an embedded `\n`. Subsequent edits used stale indices and either
        // bailed out or overwrote the wrong line.
        let tmp = make_tmp();
        let file = tmp.path().join("sample.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();

        let mut p = make_text_editor(&tmp);
        p.push_path("sample.txt");
        p.fetch();

        // Replace line 1 ("beta") with two lines.
        let old = format!("<input>{}beta</input>", tags::format_src(1));
        assert!(p.commit_edit(&old, "b1\nb2"));

        // After re-parsing, "gamma" is now at src=3. Editing it must succeed.
        let old_gamma = format!("<input>{}gamma</input>", tags::format_src(3));
        assert!(p.commit_edit(&old_gamma, "GAMMA"));

        let written = std::fs::read_to_string(&file).unwrap();
        assert_eq!(written, "alpha\nb1\nb2\nGAMMA\n");
    }

    #[test]
    fn commit_edit_insert_empty_line() {
        // Ctrl+I followed by Enter (no text) inserts a blank line at the
        // placeholder position.
        let tmp = make_tmp();
        let file = tmp.path().join("sample.txt");
        std::fs::write(&file, "alpha\ngamma\n").unwrap();

        let mut p = make_text_editor(&tmp);
        p.push_path("sample.txt");
        p.fetch();

        let old = format!("<input>{}</input>", tags::format_src_insert(1));
        assert!(p.commit_edit(&old, ""));

        let written = std::fs::read_to_string(&file).unwrap();
        assert_eq!(written, "alpha\n\ngamma\n");
    }

    #[test]
    fn commit_edit_insert_multiline() {
        // Ctrl+I, type three indented lines, Enter: they are inserted before
        // the displaced line, as typed.
        let tmp = make_tmp();
        let file = tmp.path().join("py.py");
        std::fs::write(&file, "def foo():\n    pass\n").unwrap();

        let mut p = make_text_editor(&tmp);
        p.push_path("py.py");
        p.fetch();

        let old = format!("<input>{}</input>", tags::format_src_insert(1));
        assert!(p.commit_edit(&old, "    a = 1\n    b = 2\n    c = 3"));

        let written = std::fs::read_to_string(&file).unwrap();
        assert_eq!(
            written,
            "def foo():\n    a = 1\n    b = 2\n    c = 3\n    pass\n"
        );
    }

    #[test]
    fn commit_edit_writes_first_line_to_empty_file() {
        // Repro: create file → right (open) → i (insert mode on the seeded
        // placeholder) → type → Enter. Previously commit_edit fell through all
        // four cases and returned false, leaving the file empty on disk.
        let tmp = make_tmp();
        let file = tmp.path().join("new.txt");
        std::fs::write(&file, "").unwrap();

        let mut p = make_text_editor(&tmp);
        p.push_path("new.txt");
        p.fetch();

        // Empty `old` mirrors what handle_enter_insert sends when the
        // active element is the I_PLACEHOLDER (`<input></input>` is empty).
        assert!(p.commit_edit("", "hello"));

        let written = std::fs::read_to_string(&file).unwrap();
        assert_eq!(written, "hello");
    }

    #[test]
    fn commit_edit_writes_multiline_to_empty_file() {
        // Multi-line input via Ctrl+Enter → each piece becomes its own file
        // line, including trailing/leading blank lines.
        let tmp = make_tmp();
        let file = tmp.path().join("new.txt");
        std::fs::write(&file, "").unwrap();

        let mut p = make_text_editor(&tmp);
        p.push_path("new.txt");
        p.fetch();

        assert!(p.commit_edit("", "first\n\nthird"));

        let written = std::fs::read_to_string(&file).unwrap();
        assert_eq!(written, "first\n\nthird");
    }

    #[test]
    fn commit_edit_after_first_write_can_edit_following_lines() {
        // Regression for the empty-file flow: after the placeholder commit,
        // source_lines must mirror the file so a subsequent edit lands on
        // the right line.
        let tmp = make_tmp();
        let file = tmp.path().join("new.txt");
        std::fs::write(&file, "").unwrap();

        let mut p = make_text_editor(&tmp);
        p.push_path("new.txt");
        p.fetch();

        assert!(p.commit_edit("", "first\nsecond"));

        // After the placeholder commit the file has two lines (src=0,1).
        let old = format!("<input>{}second</input>", tags::format_src(1));
        assert!(p.commit_edit(&old, "SECOND"));

        let written = std::fs::read_to_string(&file).unwrap();
        assert_eq!(written, "first\nSECOND");
    }

    #[test]
    fn commit_edit_replace_with_empty_string_blanks_line() {
        // Replacing a line with empty input keeps the slot but blanks it out.
        let tmp = make_tmp();
        let file = tmp.path().join("sample.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();

        let mut p = make_text_editor(&tmp);
        p.push_path("sample.txt");
        p.fetch();

        let old = format!("<input>{}beta</input>", tags::format_src(1));
        assert!(p.commit_edit(&old, ""));

        let written = std::fs::read_to_string(&file).unwrap();
        assert_eq!(written, "alpha\n\ngamma\n");
    }

    // ---- delete timeline emission + undo/redo -------------------------------

    #[test]
    fn delete_item_records_an_undo_with_the_file_snapshot() {
        let tmp = make_tmp();
        std::fs::write(tmp.path().join("doomed.txt"), b"important content").unwrap();
        let mut p = make_text_editor(&tmp);

        assert!(p.delete_item("<input>doomed.txt</input>"));
        assert!(!tmp.path().join("doomed.txt").exists());

        let entries = p.take_timeline_entries();
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0].label.contains("doomed.txt"),
            "{}",
            entries[0].label
        );
        match snapshot_of(&entries[0]) {
            FsSideEffect::TrashedFile {
                content_snapshot, ..
            } => {
                assert_eq!(content_snapshot, b"important content");
            }
            other => panic!("expected TrashedFile, got {other:?}"),
        }
    }

    #[test]
    fn delete_item_records_an_undo_with_the_dir_snapshot() {
        let tmp = make_tmp();
        let dir = tmp.path().join("doomed_dir");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("inner.txt"), b"nested").unwrap();
        let mut p = make_text_editor(&tmp);

        assert!(p.delete_item("<input>doomed_dir</input>"));
        assert!(!dir.exists());

        let entries = p.take_timeline_entries();
        assert_eq!(entries.len(), 1);
        assert!(matches!(
            snapshot_of(&entries[0]),
            FsSideEffect::TrashedDir { .. }
        ));
    }

    #[test]
    fn undo_of_a_file_delete_restores_it() {
        let tmp = make_tmp();
        let target = tmp.path().join("doomed.txt");
        std::fs::write(&target, b"restore me").unwrap();
        let mut p = make_text_editor(&tmp);

        assert!(p.delete_item("<input>doomed.txt</input>"));
        assert!(!target.exists());

        let entries = p.take_timeline_entries();
        let mut err = String::new();
        if let Err(e) = p.undo(&entries[0]) {
            err = e;
        }
        assert!(err.is_empty(), "undo error: {err}");
        assert_eq!(std::fs::read(&target).unwrap(), b"restore me");
    }

    #[test]
    fn undo_of_a_folder_delete_restores_the_tree() {
        let tmp = make_tmp();
        let dir = tmp.path().join("a");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("inner.txt"), b"nested").unwrap();
        let sub = dir.join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("deep.txt"), b"deeper").unwrap();
        let mut p = make_text_editor(&tmp);

        assert!(p.delete_item("<input>a</input>"));
        assert!(!dir.exists());

        let entries = p.take_timeline_entries();
        let mut err = String::new();
        if let Err(e) = p.undo(&entries[0]) {
            err = e;
        }
        assert!(err.is_empty(), "undo error: {err}");
        assert!(dir.is_dir());
        assert_eq!(std::fs::read(dir.join("inner.txt")).unwrap(), b"nested");
        assert_eq!(std::fs::read(sub.join("deep.txt")).unwrap(), b"deeper");
    }

    #[test]
    fn redo_of_a_delete_trashes_it_again() {
        let tmp = make_tmp();
        let target = tmp.path().join("doomed.txt");
        std::fs::write(&target, b"x").unwrap();
        let mut p = make_text_editor(&tmp);

        assert!(p.delete_item("<input>doomed.txt</input>"));
        let entries = p.take_timeline_entries();
        let mut err = String::new();
        if let Err(e) = p.undo(&entries[0]) {
            err = e;
        }
        assert!(target.exists());
        if let Err(e) = p.redo(&entries[0]) {
            err = e;
        }
        assert!(err.is_empty(), "redo error: {err}");
        assert!(!target.exists(), "redo deletes the file again");
    }

    #[test]
    fn delete_item_line_emits_provider_op() {
        let tmp = make_tmp();
        let file = tmp.path().join("sample.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma").unwrap();
        let mut p = make_text_editor(&tmp);
        p.push_path("sample.txt");
        p.fetch(); // prime source_lines

        let name = format!("<input>{}beta</input>", tags::format_src(1));
        assert!(p.delete_item(&name));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\ngamma");

        let entries = p.take_timeline_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].command, "texteditor.delete_line");
    }

    #[test]
    fn undo_line_delete_restores_line_verbatim() {
        // The deleted line carries indentation — undo must restore it exactly,
        // without routing through commit_edit (which re-applies indent).
        let tmp = make_tmp();
        let file = tmp.path().join("py.py");
        std::fs::write(&file, "def foo():\n    pass\n    return 1\n").unwrap();
        let mut p = make_text_editor(&tmp);
        p.push_path("py.py");
        p.fetch();

        // Delete line 1 ("    pass").
        let name = format!("<input>{}pass</input>", tags::format_src(1));
        assert!(p.delete_item(&name));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "def foo():\n    return 1\n"
        );

        let entries = p.take_timeline_entries();
        let mut err = String::new();
        if let Err(e) = p.undo(&entries[0]) {
            err = e;
        }
        assert!(err.is_empty(), "undo error: {err}");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "def foo():\n    pass\n    return 1\n"
        );
    }

    #[test]
    fn redo_line_delete_removes_line_again() {
        let tmp = make_tmp();
        let file = tmp.path().join("sample.txt");
        std::fs::write(&file, "alpha\nbeta\ngamma").unwrap();
        let mut p = make_text_editor(&tmp);
        p.push_path("sample.txt");
        p.fetch();

        let name = format!("<input>{}beta</input>", tags::format_src(1));
        assert!(p.delete_item(&name));
        let entries = p.take_timeline_entries();
        let mut err = String::new();
        if let Err(e) = p.undo(&entries[0]) {
            err = e;
        }
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\nbeta\ngamma"
        );
        if let Err(e) = p.redo(&entries[0]) {
            err = e;
        }
        assert!(err.is_empty(), "redo error: {err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\ngamma");
    }

    #[test]
    fn line_delete_undo_redo_round_trip() {
        let tmp = make_tmp();
        let file = tmp.path().join("sample.txt");
        std::fs::write(&file, "a\nb\nc").unwrap();
        let mut p = make_text_editor(&tmp);
        p.push_path("sample.txt");
        p.fetch();

        let name = format!("<input>{}b</input>", tags::format_src(1));
        assert!(p.delete_item(&name));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a\nc");

        let entries = p.take_timeline_entries();
        let mut err = String::new();
        if let Err(e) = p.undo(&entries[0]) {
            err = e;
        }
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a\nb\nc");
        if let Err(e) = p.redo(&entries[0]) {
            err = e;
        }
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a\nc");
        if let Err(e) = p.undo(&entries[0]) {
            err = e;
        }
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a\nb\nc");
        assert!(err.is_empty(), "undo/redo error: {err}");
    }

    // ---- line-insert timeline emission + undo/redo --------------------------

    #[test]
    fn commit_edit_insert_emits_provider_op() {
        let tmp = make_tmp();
        let file = tmp.path().join("sample.txt");
        std::fs::write(&file, "alpha\ngamma").unwrap();
        let mut p = make_text_editor(&tmp);
        p.push_path("sample.txt");
        p.fetch();

        let old = format!("<input>{}</input>", tags::format_src_insert(1));
        assert!(p.commit_edit(&old, "beta"));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\nbeta\ngamma"
        );

        let entries = p.take_timeline_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].command, "texteditor.insert_lines");
    }

    #[test]
    fn undo_line_insert_removes_line() {
        let tmp = make_tmp();
        let file = tmp.path().join("sample.txt");
        std::fs::write(&file, "alpha\ngamma").unwrap();
        let mut p = make_text_editor(&tmp);
        p.push_path("sample.txt");
        p.fetch();

        let old = format!("<input>{}</input>", tags::format_src_insert(1));
        assert!(p.commit_edit(&old, "beta"));
        let entries = p.take_timeline_entries();
        let mut err = String::new();
        if let Err(e) = p.undo(&entries[0]) {
            err = e;
        }
        assert!(err.is_empty(), "undo error: {err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\ngamma");
    }

    #[test]
    fn redo_line_insert_re_adds_line() {
        let tmp = make_tmp();
        let file = tmp.path().join("sample.txt");
        std::fs::write(&file, "alpha\ngamma").unwrap();
        let mut p = make_text_editor(&tmp);
        p.push_path("sample.txt");
        p.fetch();

        let old = format!("<input>{}</input>", tags::format_src_insert(1));
        assert!(p.commit_edit(&old, "beta"));
        let entries = p.take_timeline_entries();
        let mut err = String::new();
        if let Err(e) = p.undo(&entries[0]) {
            err = e;
        }
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\ngamma");
        if let Err(e) = p.redo(&entries[0]) {
            err = e;
        }
        assert!(err.is_empty(), "redo error: {err}");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\nbeta\ngamma"
        );
    }

    #[test]
    fn line_insert_undo_redo_multiline() {
        // A multi-line insert (Ctrl+Enter input with embedded `\n`) must undo
        // and redo as one atomic step covering every inserted line.
        let tmp = make_tmp();
        let file = tmp.path().join("sample.txt");
        std::fs::write(&file, "alpha\ngamma").unwrap();
        let mut p = make_text_editor(&tmp);
        p.push_path("sample.txt");
        p.fetch();

        let old = format!("<input>{}</input>", tags::format_src_insert(1));
        assert!(p.commit_edit(&old, "b1\nb2"));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\nb1\nb2\ngamma"
        );

        let entries = p.take_timeline_entries();
        let mut err = String::new();
        if let Err(e) = p.undo(&entries[0]) {
            err = e;
        }
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\ngamma");
        if let Err(e) = p.redo(&entries[0]) {
            err = e;
        }
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "alpha\nb1\nb2\ngamma"
        );
        assert!(err.is_empty(), "undo/redo error: {err}");
    }

    #[test]
    fn insert_blank_line_undo_redo() {
        // Ctrl+I then Enter with no text inserts one blank line; undo/redo must
        // treat the empty payload as exactly one line.
        let tmp = make_tmp();
        let file = tmp.path().join("sample.txt");
        std::fs::write(&file, "alpha\ngamma\n").unwrap();
        let mut p = make_text_editor(&tmp);
        p.push_path("sample.txt");
        p.fetch();

        let old = format!("<input>{}</input>", tags::format_src_insert(1));
        assert!(p.commit_edit(&old, ""));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\n\ngamma\n");

        let entries = p.take_timeline_entries();
        let mut err = String::new();
        if let Err(e) = p.undo(&entries[0]) {
            err = e;
        }
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\ngamma\n");
        if let Err(e) = p.redo(&entries[0]) {
            err = e;
        }
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "alpha\n\ngamma\n");
        assert!(err.is_empty(), "undo/redo error: {err}");
    }

    #[test]
    fn commit_edit_first_line_of_empty_file_emits_provider_op() {
        // The first write into an empty file is a line insert, not a file
        // creation — it must emit an `insert_lines` entry.
        let tmp = make_tmp();
        let file = tmp.path().join("new.txt");
        std::fs::write(&file, "").unwrap();
        let mut p = make_text_editor(&tmp);
        p.push_path("new.txt");
        p.fetch();

        assert!(p.commit_edit("", "hello"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello");

        let entries = p.take_timeline_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].command, "texteditor.insert_lines");
    }

    #[test]
    fn first_line_write_undo_redo() {
        let tmp = make_tmp();
        let file = tmp.path().join("new.txt");
        std::fs::write(&file, "").unwrap();
        let mut p = make_text_editor(&tmp);
        p.push_path("new.txt");
        p.fetch();

        assert!(p.commit_edit("", "hello"));
        let entries = p.take_timeline_entries();
        let mut err = String::new();
        if let Err(e) = p.undo(&entries[0]) {
            err = e;
        }
        assert!(err.is_empty(), "undo error: {err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "");
        if let Err(e) = p.redo(&entries[0]) {
            err = e;
        }
        assert!(err.is_empty(), "redo error: {err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello");
    }

    // ---- .ff files ---------------------------------------------------------

    fn ff_leaf(content: &str) -> ff::Node {
        ff::Node::new(content)
    }

    fn ff_branch(content: &str, children: Vec<ff::Node>) -> ff::Node {
        let mut node = ff::Node::new(content);
        node.children = children;
        node.rehash();
        node
    }

    /// `a`, `b:` holding `c` and `d`, then `e` (pre-order 0 to 4).
    fn ff_sample() -> ff::Node {
        ff_branch(
            "",
            vec![
                ff_leaf("a"),
                ff_branch("b:", vec![ff_leaf("c"), ff_leaf("d")]),
                ff_leaf("e"),
            ],
        )
    }

    fn read_ff(path: &Path) -> ff::Node {
        let root = ff::decode(&std::fs::read(path).unwrap()).unwrap();
        assert!(root.verify(), "every hash on disk is the right one");
        root
    }

    fn top_contents(root: &ff::Node) -> Vec<&str> {
        root.children.iter().map(|c| c.content.as_str()).collect()
    }

    /// The row every `.ff` level opens with.
    fn ff_header() -> FfonElement {
        FfonElement::new_obj("header")
    }

    /// An editor open on `notes.ff` holding `root`.
    fn open_ff(tmp: &TempDir, root: &ff::Node) -> (TextEditorProvider, PathBuf) {
        let file = tmp.path().join("notes.ff");
        std::fs::write(&file, ff::encode(root).unwrap()).unwrap();
        let mut p = make_text_editor(tmp);
        p.push_path("notes.ff");
        p.fetch();
        (p, file)
    }

    /// The app restores a tab by setting its saved path, one level at a time.
    /// A path into a file runs past the file into its levels, which are not on
    /// disk: the editor took the whole of it for a folder that does not exist,
    /// the levels came back empty, and the tab reopened at the top of the file.
    #[test]
    fn ff_a_path_into_a_file_opens_the_levels_inside_it() {
        let tmp = make_tmp();
        let root = ff_branch(
            "",
            vec![
                ff_leaf("a"),
                ff_branch("b:", vec![ff_leaf("c"), ff_branch("d:", vec![ff_leaf("deep")])]),
            ],
        );
        let (mut p, _file) = open_ff(&tmp, &root);
        p.push_path("b:");
        p.push_path("d:");
        let path = p.current_path().to_owned();
        let rows = p.fetch();
        assert!(format!("{rows:?}").contains("deep"), "{rows:?}");

        let mut restored = make_text_editor(&tmp);
        restored.set_current_path(&path);
        assert_eq!(restored.current_path(), path);
        assert_eq!(restored.fetch(), rows);
        restored.pop_path();
        p.pop_path();
        assert_eq!(restored.fetch(), p.fetch(), "one level up is `b:`");
    }

    /// [`ff_a_path_into_a_file_opens_the_levels_inside_it`] for a text file.
    #[test]
    fn a_path_into_a_text_file_opens_the_level_inside_it() {
        let tmp = make_tmp();
        std::fs::write(tmp.path().join("data.txt"), "section:\n{\n  child\n}").unwrap();
        let mut p = make_text_editor(&tmp);
        p.push_path("data.txt");
        p.push_path("section:");
        let path = p.current_path().to_owned();
        let rows = p.fetch();
        assert!(!rows.is_empty());

        let mut restored = make_text_editor(&tmp);
        restored.set_current_path(&path);
        assert_eq!(restored.current_path(), path);
        assert_eq!(restored.fetch(), rows);
    }

    #[test]
    fn ff_empty_file_opens_and_every_enter_writes_the_tree() {
        let tmp = make_tmp();
        let file = tmp.path().join("notes.ff");
        std::fs::write(&file, b"").unwrap();
        let mut p = make_text_editor(&tmp);
        p.push_path("notes.ff");
        assert_eq!(
            p.fetch(),
            [ff_header()],
            "an empty file is an empty root, under its header"
        );

        // The `i` placeholder of the empty level.
        assert!(p.commit_edit("", "hello"));
        let first = read_ff(&file);
        assert_eq!(top_contents(&first), ["hello"]);
        assert_eq!(first.children[0].hash, ff::Node::new("hello").hash);
        assert_eq!(
            p.fetch(),
            [
                ff_header(),
                FfonElement::new_str("<input><src=0>hello</input>")
            ]
        );

        assert!(p.commit_edit("<input><src=0>hello</input>", "world"));
        let second = read_ff(&file);
        assert_eq!(top_contents(&second), ["world"]);
        assert_ne!(
            second.hash, first.hash,
            "the root hash follows every change"
        );
    }

    #[test]
    fn ff_trailing_colon_makes_a_branch_to_enter_and_fill() {
        let tmp = make_tmp();
        let (mut p, file) = open_ff(&tmp, &ff_branch("", vec![ff_leaf("x")]));

        assert!(p.commit_edit("<input><src=0>x</input>", "groceries:"));
        let top = p.fetch();
        let obj = top[1].as_obj().expect("a trailing : is a branch");
        assert_eq!(obj.key, "<input><src=0>groceries:</input>");

        // Right: the app passes the element's input, `<src=N>` and all.
        p.push_path("<src=0>groceries:");
        assert_eq!(p.fetch(), [ff_header()]);
        assert!(p.commit_edit("", "milk"));
        assert_eq!(
            p.fetch(),
            [
                ff_header(),
                FfonElement::new_str("<input><src=1>milk</input>")
            ]
        );
        assert!(p.commit_edit("<input><srcins=2></input>", "bread"));
        assert_eq!(
            p.fetch(),
            [
                ff_header(),
                FfonElement::new_str("<input><src=1>milk</input>"),
                FfonElement::new_str("<input><src=2>bread</input>"),
            ]
        );

        let root = read_ff(&file);
        assert_eq!(top_contents(&root.children[0]), ["milk", "bread"]);

        p.pop_path();
        let top = p.fetch();
        assert_eq!(
            top[1].as_obj().unwrap().children,
            [
                ff_header(),
                FfonElement::new_str("<input><src=1>milk</input>"),
                FfonElement::new_str("<input><src=2>bread</input>"),
            ],
            "a branch the app holds opens with its header too"
        );
    }

    #[test]
    fn ff_insert_after_a_node_with_children_lands_beside_it() {
        let tmp = make_tmp();
        let (mut p, file) = open_ff(&tmp, &ff_sample());
        // Ctrl+A on `b:` (node 1) asks for <srcins=2>, which is `c`, inside it.
        assert!(p.commit_edit("<input><srcins=2></input>", "x"));
        assert_eq!(top_contents(&read_ff(&file)), ["a", "b:", "x", "e"]);
        // Ctrl+I on `a`.
        assert!(p.commit_edit("<input><srcins=0></input>", "first"));
        assert_eq!(
            top_contents(&read_ff(&file)),
            ["first", "a", "b:", "x", "e"]
        );
    }

    #[test]
    fn ff_edits_inside_a_branch_address_the_whole_tree() {
        let tmp = make_tmp();
        let (mut p, file) = open_ff(&tmp, &ff_sample());
        p.push_path("<src=1>b:");
        assert_eq!(
            p.fetch(),
            [
                ff_header(),
                FfonElement::new_str("<input><src=2>c</input>"),
                FfonElement::new_str("<input><src=3>d</input>"),
            ]
        );
        assert!(p.commit_edit("<input><src=3>d</input>", "D"));
        let root = read_ff(&file);
        assert_eq!(top_contents(&root.children[1]), ["c", "D"]);
        assert_eq!(top_contents(&root), ["a", "b:", "e"]);
    }

    #[test]
    fn ff_a_newline_stays_inside_one_node() {
        let tmp = make_tmp();
        let (mut p, file) = open_ff(&tmp, &ff_sample());
        assert!(p.commit_edit("<input><src=0>a</input>", "two\nlines"));
        assert_eq!(top_contents(&read_ff(&file)), ["two\nlines", "b:", "e"]);
    }

    #[test]
    fn ff_delete_takes_the_subtree_and_undo_redo_replay_it() {
        let tmp = make_tmp();
        let (mut p, file) = open_ff(&tmp, &ff_sample());
        let original = std::fs::read(&file).unwrap();

        assert!(p.delete_item("<input><src=1>b:</input>"));
        let deleted = std::fs::read(&file).unwrap();
        assert_eq!(top_contents(&read_ff(&file)), ["a", "e"]);

        let entries = p.take_timeline_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(p.undo(&entries[0]), Ok(()));
        assert_eq!(std::fs::read(&file).unwrap(), original);
        assert_eq!(p.redo(&entries[0]), Ok(()));
        assert_eq!(std::fs::read(&file).unwrap(), deleted);
    }

    #[test]
    fn ff_insert_undo_redo_replay_it() {
        let tmp = make_tmp();
        let (mut p, file) = open_ff(&tmp, &ff_sample());
        let original = std::fs::read(&file).unwrap();

        assert!(p.commit_edit("<input><srcins=2></input>", "x"));
        let inserted = std::fs::read(&file).unwrap();
        let entries = p.take_timeline_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(p.undo(&entries[0]), Ok(()));
        assert_eq!(std::fs::read(&file).unwrap(), original);
        assert_eq!(p.redo(&entries[0]), Ok(()));
        assert_eq!(std::fs::read(&file).unwrap(), inserted);
    }

    #[test]
    fn ff_undo_of_an_insert_through_commit_edit_removes_that_node() {
        let tmp = make_tmp();
        let (mut p, file) = open_ff(&tmp, &ff_sample());
        assert!(p.commit_edit("<input><srcins=2></input>", "x"));
        // `x` is node 4 now: after b's two children.
        assert!(p.commit_edit("<input><src=4>x</input>", "<input><srcins=2></input>"));
        assert_eq!(top_contents(&read_ff(&file)), ["a", "b:", "e"]);
    }

    #[test]
    fn ff_more_than_255_bytes_is_refused_and_said() {
        let tmp = make_tmp();
        let (mut p, file) = open_ff(&tmp, &ff_sample());
        let before = std::fs::read(&file).unwrap();
        assert!(!p.commit_edit("<input><src=0>a</input>", &"x".repeat(256)));
        assert_eq!(std::fs::read(&file).unwrap(), before);
        let error = p.poll().error.expect("the header says why");
        assert!(error.contains("255"), "{error}");
        assert!(p.commit_edit("<input><src=0>a</input>", &"x".repeat(255)));
    }

    #[test]
    fn ff_a_damaged_file_opens_as_one_row_saying_why() {
        let tmp = make_tmp();
        let file = tmp.path().join("notes.ff");
        let bytes = ff::encode(&ff_sample()).unwrap();
        std::fs::write(&file, &bytes[..bytes.len() - 3]).unwrap();
        let mut p = make_text_editor(&tmp);
        p.push_path("notes.ff");
        let rows = p.fetch();
        assert_eq!(rows.len(), 1);
        let row = rows[0].as_str().unwrap();
        assert!(row.contains("not a valid .ff file"), "{row}");
        assert!(!p.commit_edit("", "anything"), "nothing to write into");
        assert_eq!(std::fs::read(&file).unwrap(), &bytes[..bytes.len() - 3]);
    }

    #[test]
    fn ff_a_wrong_hash_opens_with_a_warning_and_saving_mends_it() {
        let tmp = make_tmp();
        let file = tmp.path().join("notes.ff");
        let mut bytes = ff::encode(&ff_sample()).unwrap();
        bytes[0] ^= 1; // the root's hash
        std::fs::write(&file, &bytes).unwrap();
        let mut p = make_text_editor(&tmp);
        p.push_path("notes.ff");
        assert_eq!(p.fetch().len(), 4, "the header and three nodes");
        let error = p.poll().error.expect("a warning");
        assert!(error.contains("hashes do not match"), "{error}");

        assert!(p.commit_edit("<input><src=0>a</input>", "A"));
        read_ff(&file); // verifies
    }

    fn sha256_row(hash: &[u8; 32]) -> FfonElement {
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        FfonElement::new_str(format!("sha256: {hex}"))
    }

    #[test]
    fn ff_the_top_header_holds_the_root_hash_and_follows_every_edit() {
        let tmp = make_tmp();
        let (mut p, file) = open_ff(&tmp, &ff_sample());
        p.push_path("header");
        assert_eq!(p.fetch(), [sha256_row(&ff_sample().hash)]);
        p.pop_path();

        // An edit deep down changes the hash the top header shows.
        p.push_path("<src=1>b:");
        assert!(p.commit_edit("<input><src=3>d</input>", "D"));
        p.pop_path();
        p.push_path("header");
        let root = read_ff(&file);
        assert_ne!(root.hash, ff_sample().hash);
        assert_eq!(p.fetch(), [sha256_row(&root.hash)]);
    }

    #[test]
    fn ff_a_nested_header_holds_the_hash_of_its_node() {
        let tmp = make_tmp();
        let (mut p, _file) = open_ff(&tmp, &ff_sample());
        p.push_path("<src=1>b:");
        p.push_path("header");
        assert_eq!(p.fetch(), [sha256_row(&ff_sample().children[1].hash)]);
        // Nothing can be added inside a header.
        assert!(!p.commit_edit("", "x"));
        p.pop_path();
        assert_eq!(
            p.fetch().len(),
            3,
            "back in b: with its header and two nodes"
        );
    }

    #[test]
    fn ff_a_node_called_header_is_still_a_node() {
        let tmp = make_tmp();
        let root = ff_branch("", vec![ff_branch("header", vec![ff_leaf("inside")])]);
        let (mut p, _file) = open_ff(&tmp, &root);
        // A node's input carries its `<src=N>`. The header row's does not.
        p.push_path("<src=0>header");
        assert_eq!(
            p.fetch(),
            [
                ff_header(),
                FfonElement::new_str("<input><src=1>inside</input>")
            ]
        );
    }

    #[test]
    fn ff_the_header_row_cannot_be_edited_or_deleted() {
        let tmp = make_tmp();
        let (mut p, file) = open_ff(&tmp, &ff_sample());
        let before = std::fs::read(&file).unwrap();
        assert!(!p.commit_edit("header", "renamed"));
        assert!(!p.delete_item("header"));
        assert_eq!(std::fs::read(&file).unwrap(), before);
    }
}

/// The tutorial's paragraphs about this plugin are the plugin's own:
/// `texteditor-tutorial`, then `texteditor-tutorial-2` and so on. The tutorial reads them
/// from the installed `locales/*.ftl`, so every language needs the same ones.
#[cfg(test)]
mod tutorial_text_tests {
    const LOCALES: [(&str, &str); 4] = [
        ("en-US", include_str!("../locales/en-US.ftl")),
        ("nl-BE", include_str!("../locales/nl-BE.ftl")),
        ("fr-BE", include_str!("../locales/fr-BE.ftl")),
        ("de-BE", include_str!("../locales/de-BE.ftl")),
    ];

    /// The plugin's name, kept apart from the `-tutorial` suffix so no
    /// half-built id appears quoted in the source.
    const NAME: &str = "texteditor";

    fn tutorial_id(line: &str) -> Option<&str> {
        let id = line.split_once(" = ")?.0;
        let base = format!("{NAME}-tutorial");
        (id == base || id.starts_with(&format!("{base}-"))).then_some(id)
    }

    fn tutorial_ids(ftl: &str) -> Vec<&str> {
        ftl.lines().filter_map(tutorial_id).collect()
    }

    #[test]
    fn every_language_has_the_same_tutorial_leaves() {
        let en = tutorial_ids(LOCALES[0].1);
        assert_eq!(en, ["texteditor-tutorial"], "en-US's tutorial leaves");
        for (locale, ftl) in &LOCALES[1..] {
            assert_eq!(tutorial_ids(ftl), en, "{locale} has drifted from en-US");
        }
    }
}
