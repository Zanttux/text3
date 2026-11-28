//! Window chrome: menus, tab bar, status bar, file dialogs and
//! confirmations.

use std::collections::HashMap;
use std::path::Path;

use eframe::egui::{self, ViewportCommand};
use text3_app::editor::Restored;
use text3_app::syntax::{CharSpan, MAX_BYTES, SyntaxWorker};
use text3_app::workspace::{TabId, Workspace};
use text3_app::{Command, Editor, SaveOutcome};
use text3_core::LineEnding;
use text3_core::Revision;
use text3_i18n::{FluentValue, Localizer};
use text3_io::{LoadError, TextEncoding};
use text3_language::Lang;

use crate::find::{FindBar, FindEvent};
use crate::focus::{Edge, Entry as FocusEntry, Region};
use crate::menu::{self, MenuAction};
use crate::palette::{Entry, Palette};
use crate::preview;
use crate::theme;
use crate::view::{ContextLabels, EditorView, ViewOptions};
use text3_app::macros::{MacroError, MacroStore};
use text3_app::panes::{Direction, MAX_PANES, Panes};
use text3_app::settings::{FONT_SIZE_DEFAULT, Settings, ThemeChoice};
use text3_app::update::{Status as UpdateStatus, UpdateError};

/// Background font work for the editor font setting.
enum FontMsg {
    Families(text3_app::fonts::FontList),
    /// (family, regular face bytes and index); `None` if not found.
    Loaded(String, Option<(Vec<u8>, u32)>),
}

/// Use `face` as the first monospace font (the built-in fonts stay as
/// fallbacks for missing glyphs); `None` restores the default.
fn apply_editor_font(ctx: &egui::Context, face: Option<(Vec<u8>, u32)>) {
    let mut fonts = egui::FontDefinitions::default();
    if let Some((bytes, index)) = face {
        let mut data = egui::FontData::from_owned(bytes);
        data.index = index;
        fonts
            .font_data
            .insert("text3-editor".into(), std::sync::Arc::new(data));
        if let Some(mono) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
            mono.insert(0, "text3-editor".into());
        }
    }
    ctx.set_fonts(fonts);
}

/// Sections of the Settings window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    General,
    Editor,
    Appearance,
    Shortcuts,
}

/// Upper bound for Play Macro Multiple Times.
const MACRO_TIMES_MAX: usize = 10_000;

mod bench;
mod default_apps;
mod goto_anything;

/// Markdown outline (level, title, line) for a tab at a revision.
type HeadingCache = ((TabId, Revision), Vec<(u8, String, usize)>);

/// A replace-all waiting for confirmation.
struct ReplacePreview {
    tab: TabId,
    revision: Revision,
    tx: text3_core::Transaction,
    count: usize,
    /// (line, before, after) for the first changed lines.
    lines: Vec<(usize, String, String)>,
}

/// Lines shown in the replace-all preview.
const PREVIEW_LINES: usize = 200;

/// The right side of a comparison: another tab, or fixed text such as
/// the saved file or the clipboard (read-only).
enum Side {
    Tab(TabId),
    Text { label: String, text: String },
}

/// An open side-by-side comparison of the active tab with another side.
struct Compare {
    left: TabId,
    right: Side,
    revs: (Revision, Revision),
    diff: text3_tools::diff::Diff,
    /// The texts the diff was computed from (refreshed when a side's
    /// revision changes, not every frame).
    texts: (std::sync::Arc<String>, std::sync::Arc<String>),
    /// Lines differing only in whitespace count as equal.
    ignore_whitespace: bool,
}

/// The encoding picker's purpose.
enum EncodingPick {
    /// Reopen this file with the chosen encoding.
    Reopen {
        path: std::path::PathBuf,
        /// First bytes of the file, for the preview.
        head: Vec<u8>,
        selected: usize,
        error: Option<String>,
        /// The encoding guessed from the file's bytes, if any.
        suggested: Option<TextEncoding>,
    },
    /// Save the active document with the chosen encoding.
    Save {
        selected: usize,
        error: Option<String>,
    },
}

/// Encodings offered: Unicode first, then legacy ones.
fn encoding_choices(reopen: bool) -> Vec<TextEncoding> {
    let mut v = vec![TextEncoding::Utf8];
    if !reopen {
        v.extend([
            TextEncoding::Utf8Bom,
            TextEncoding::Utf16LeBom,
            TextEncoding::Utf16BeBom,
        ]);
    }
    v.extend([TextEncoding::Utf16Le, TextEncoding::Utf16Be]);
    v.extend(text3_io::LEGACY.iter().map(|e| TextEncoding::Legacy(e)));
    v
}

/// Keyboard focus id of a tab's CSV grid.
fn grid_id(id: TabId) -> egui::Id {
    egui::Id::unique(("text3-csv-grid", id))
}

/// The Markdown preview of tab `id` in pane slot `slot`, which takes
/// keyboard focus when the pane shows only the preview.
fn preview_focus_id(id: TabId, slot: usize) -> egui::Id {
    egui::Id::unique(("text3-md-preview", id, slot))
}

/// Keyboard focus id of a tab's JSON or XML tree.
fn tree_id(id: TabId) -> egui::Id {
    egui::Id::unique(("text3-json-tree", id))
}

/// Parsed data behind a structured view, for one document revision.
enum Structured {
    /// The table, and whether its file is read-only on disk (no grid edits).
    Csv(text3_tools::csv::Table, bool),
    /// The editable tree, and whether its file is read-only on disk.
    Json(
        Result<text3_tools::json::Doc, text3_tools::json::JsonError>,
        bool,
    ),
    /// The editable tree, and whether its file is read-only on disk.
    Xml(
        Result<text3_tools::xml::Doc, text3_tools::markup::TreeError>,
        bool,
    ),
    /// The editable tree, and whether its file is read-only on disk.
    Yaml(
        Result<text3_tools::yaml::Doc, text3_tools::markup::TreeError>,
        bool,
    ),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DataKind {
    Csv,
    Tsv,
    Json,
    Xml,
    Yaml,
}

/// How a Markdown tab is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MdMode {
    Source,
    Split,
    Preview,
}

/// What an editor pane shows for its tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneContent {
    Editor,
    Preview,
    EditorAndPreview,
    Structured,
}

/// The content of a pane: the structured view when it is on for a data
/// file, else the Markdown mode (`md` is `None` for other files).
/// How long hot exit waits for one tab's text to reach disk before asking
/// about that tab instead.
const HOT_EXIT_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Width of the strip around a pane divider that can be dragged.
const DIVIDER_HIT: f32 = 6.0;

/// Smallest size dragging a divider leaves a pane, where it fits.
const MIN_PANE_PX: f32 = 120.0;

fn pane_content(structured: bool, md: Option<MdMode>) -> PaneContent {
    if structured {
        return PaneContent::Structured;
    }
    match md {
        Some(MdMode::Preview) => PaneContent::Preview,
        Some(MdMode::Split) => PaneContent::EditorAndPreview,
        Some(MdMode::Source) | None => PaneContent::Editor,
    }
}

/// What to do once a tab's unsaved changes are resolved.
#[derive(Debug, Clone, Copy)]
enum Then {
    /// Close this tab.
    CloseTab(TabId),
    /// Quit: close dirty tabs one by one, then the window.
    Quit,
}

/// A dialog button's follow-up, run after the dialog closes.
#[derive(Debug, Clone, Copy)]
enum Action {
    PurgeHistory,
    ReplaceInFiles,
    Revert,
    SaveThen(Then),
    Proceed(Then),
    SaveAs,
    Overwrite,
}

#[derive(Debug, Clone)]
enum Dialog {
    Unsaved(Then),
    /// Confirm discarding unsaved changes to reload the file from disk.
    ConfirmRevert,
    Conflict,
    ReadOnly,
    Error(String),
    /// Confirm opening a web link in the system browser.
    OpenLink(String),
    /// A newer release exists: (version, release page).
    Update(String, String),
    /// Confirm Replace in Files: (matches, files).
    ConfirmReplaceFiles(usize, usize),
    /// Confirm deleting all local history.
    ConfirmPurge,
}

pub struct Shell {
    ws: Workspace,
    views: HashMap<TabId, EditorView>,
    l10n: Localizer,
    dialog: Option<Dialog>,
    status: Option<(String, bool)>,
    title: String,
    allow_close: bool,
    find: FindBar,
    folder: crate::folderpanel::FolderPanel,
    syntax: SyntaxWorker,
    /// Latest highlight result and the revision requested last, per tab.
    spans: HashMap<TabId, (Revision, Vec<CharSpan>)>,
    requested: HashMap<TabId, Revision>,
    /// Character and word counts (status bar), counted in the background.
    counts: text3_app::counts::Counter,
    /// The counts summary was open last frame (counts large files too).
    counts_open: bool,
    session_file: Option<std::path::PathBuf>,
    settings: Settings,
    settings_dir: Option<std::path::PathBuf>,
    settings_open: bool,
    history: Option<text3_io::history::HistoryStore>,
    history_open: bool,
    /// The last recorded macro.
    macro_cmds: Vec<Command>,
    /// Saved, named macros (the `macros` file in the config folder).
    saved_macros: MacroStore,
    replace_preview: Option<ReplacePreview>,
    about_open: bool,
    /// The Text³ lockup shown in About (decoded on first open).
    about_logo: Option<egui::TextureHandle>,
    /// Markdown headings for Go to Heading, per tab and revision.
    headings: Option<HeadingCache>,
    /// Goto Anything's file listing and caches.
    goto: goto_anything::GotoState,
    /// The Default apps window, when open (#164).
    default_apps: Option<default_apps::DefaultApps>,
    /// The Markdown outline side panel is shown.
    outline_open: bool,
    /// Languages chosen by hand per tab (`None` = plain text).
    lang_override: HashMap<TabId, Option<Lang>>,
    /// Section shown in Settings.
    settings_tab: SettingsTab,
    /// Give the selected section tab keyboard focus (Settings opened).
    settings_focus: bool,
    /// Filter text of the shortcut list in Settings.
    shortcut_filter: String,
    /// Installed monospaced families (None until scanned).
    font_families: Option<text3_app::fonts::FontList>,
    /// Results of background font work.
    font_rx: Option<std::sync::mpsc::Receiver<FontMsg>>,
    /// The editor font requested last, to apply only the latest result.
    font_wanted: Option<String>,
    /// Folders outside a document's own folder the user allowed images
    /// from (this session only).
    allowed_image_dirs: Vec<std::path::PathBuf>,
    /// Scroll the outline to the current heading on the next frame.
    outline_follow: bool,
    recent: text3_app::recent::Recent,
    /// `.editorconfig` indentation per tab, keyed by the file it was read for.
    tab_indent: HashMap<TabId, (Option<std::path::PathBuf>, text3_app::editorconfig::Indent)>,
    /// Split editor: the panes, each showing a tab, and which has focus
    /// (its tab is the active tab). `None` without a split.
    split: Option<Panes<TabId>>,
    /// Display state of split panes in slots 1 and up, per tab (slot 0
    /// uses `views`, like the single area).
    pane_views: HashMap<(usize, TabId), EditorView>,
    log: Option<crate::logview::LogView>,
    /// For work started outside a frame (e.g. opening a file).
    ctx: egui::Context,
    /// While a split pane is drawn: its editor's accessible name, which
    /// says which pane it is.
    pane_ax: Option<String>,
    /// A command ran from the palette: give the document keyboard focus
    /// back if nothing else took it.
    refocus: bool,
    /// A running update check and whether the user asked for it.
    update: Option<(
        std::sync::mpsc::Receiver<Result<UpdateStatus, UpdateError>>,
        bool,
    )>,
    /// Command whose new shortcut is being recorded in Settings.
    recording: Option<MenuAction>,
    /// Commands that lost their shortcut to the last recorded one.
    shortcut_moved: Vec<String>,
    /// Result of the last Notepad++ shortcut import (text, is error).
    shortcut_import: Option<(String, bool)>,
    palette: Palette,
    compare: Option<Compare>,
    encoding_pick: Option<EncodingPick>,
    structured: std::collections::HashSet<TabId>,
    structured_cache: HashMap<TabId, (Revision, Option<char>, Structured)>,
    csv_delimiter: HashMap<TabId, char>,
    csv_grids: HashMap<TabId, crate::structured::GridState>,
    trees: HashMap<TabId, crate::structured::TreeState>,
    compare_pick: bool,
    md_modes: HashMap<TabId, MdMode>,
    md_cache: HashMap<TabId, (Revision, std::sync::Arc<Vec<text3_markdown::Block>>)>,
    /// Caret line the preview was last scrolled to, per tab and pane.
    md_synced: HashMap<(TabId, usize), usize>,
    /// Heading anchor to scroll the preview to, per tab.
    md_anchor: HashMap<TabId, String>,
    images: std::cell::RefCell<crate::imagecache::ImageCache>,
    applied_theme: Option<(ThemeChoice, Option<String>)>,
    /// Bundled and user colour themes, and user theme files that failed.
    themes: (Vec<crate::themes::ThemeEntry>, Vec<(String, String)>),
    /// Set once the session was recorded for the current quit attempt.
    quit_recorded: bool,
    /// Tabs whose unsaved text hot exit kept for the next start during the
    /// current quit attempt, at that revision: they are not asked about
    /// unless edited since.
    hot_exited: Vec<(TabId, Revision)>,
    /// Marks this instance as running to others sharing the recovery folder.
    owner: Option<text3_io::recovery::OwnerLock>,
    /// The tab the strip last scrolled into view (a new or switched-to
    /// tab past the edge of an overflowing strip is brought into view).
    tab_in_view: Option<TabId>,
    /// Keyboard focus between regions (F6, Tab at region edges).
    focus: crate::focus::FocusCycle,
    #[cfg(target_os = "macos")]
    native_menu: Option<menu::NativeMenu>,
    /// macOS: the native menu is built once the first frame is on screen
    /// (building it takes about 12 ms); until then no in-window menu bar.
    #[cfg(target_os = "macos")]
    menu_pending: bool,
    /// `TEXT3_BENCH`: the workload being measured (shell/bench.rs).
    bench: Option<bench::Driver>,
}

impl Shell {
    /// Install the native menu bar where supported. Set
    /// `TEXT3_IN_WINDOW_MENU=1` (or `SANAIN_IN_WINDOW_MENU=1`) to force the
    /// in-window menu.
    /// macOS: use the native menu bar. It is built at the start of the
    /// second frame ([`Self::build_pending_menu`]), so the first frame is
    /// not held up.
    pub fn install_menu(&mut self, ctx: &egui::Context) {
        #[cfg(target_os = "macos")]
        if text3_app::migrate::var_os("IN_WINDOW_MENU").is_none() {
            self.menu_pending = true;
            // Make sure the second frame comes without waiting for input.
            ctx.request_repaint();
        }
        #[cfg(not(target_os = "macos"))]
        let _ = ctx;
    }

    /// Build the native menu asked for by [`Self::install_menu`]. If that
    /// fails, the in-window menu bar is shown from this frame on.
    #[cfg(target_os = "macos")]
    fn build_pending_menu(&mut self, ctx: &egui::Context) {
        if !std::mem::take(&mut self.menu_pending) {
            return;
        }
        let ctx = ctx.clone();
        let label = |id: &str| self.t(id, &[]);
        self.native_menu =
            menu::NativeMenu::install(&label, &self.settings.keymap, move || ctx.request_repaint());
    }

    fn has_native_menu(&self) -> bool {
        #[cfg(target_os = "macos")]
        return self.native_menu.is_some() || self.menu_pending;
        #[cfg(not(target_os = "macos"))]
        false
    }

    /// Indentation text and width for a tab: `.editorconfig` where it
    /// configures them, otherwise the editor settings.
    fn tab_indent(&mut self, id: TabId) -> (String, usize) {
        let path = self
            .ws
            .position(id)
            .and_then(|i| self.ws.tabs()[i].editor.file().map(|f| f.path.clone()));
        let fresh = self.tab_indent.get(&id).is_some_and(|(p, _)| *p == path);
        if !fresh {
            let indent = path
                .as_deref()
                .map(text3_app::editorconfig::indent_for)
                .unwrap_or_default();
            self.tab_indent.insert(id, (path, indent));
        }
        let indent = self
            .tab_indent
            .get(&id)
            .map(|(_, i)| *i)
            .unwrap_or_default();
        let width = indent.size.unwrap_or(usize::from(self.settings.tab_width));
        (
            indent.text(
                !self.settings.insert_spaces,
                usize::from(self.settings.tab_width),
            ),
            width,
        )
    }

    /// Remember a file in Open Recent.
    fn note_recent(&mut self, path: &Path) {
        self.recent.push(path);
        if let Some(dir) = &self.settings_dir {
            let _ = self.recent.save(dir);
            text3_app::bookmarks::remember(dir, path);
        }
    }

    /// Load the recent-files list and add the files open at startup.
    pub fn init_recent(&mut self) {
        if let Some(dir) = &self.settings_dir {
            self.recent = text3_app::recent::Recent::load(dir);
        }
        let paths: Vec<std::path::PathBuf> = self
            .ws
            .tabs()
            .iter()
            .filter_map(|t| t.editor.file().map(|f| f.path.clone()))
            .collect();
        for p in paths.iter().rev() {
            self.note_recent(p);
        }
    }

    /// Where saved versions are kept (`None` disables local history).
    /// Show a one-off message in the status bar.
    pub fn notify(&mut self, message: String) {
        self.status = Some((message, false));
    }

    /// A localized message without arguments.
    pub fn tr(&self, id: &str) -> String {
        self.t(id, &[])
    }

    pub fn set_history(&mut self, store: Option<text3_io::history::HistoryStore>) {
        self.history = store;
    }

    /// Show a split layout restored from the session.
    pub fn set_split(&mut self, split: Option<Panes<TabId>>) {
        self.split = split.filter(|s| s.is_split());
    }

    pub fn new(
        ws: Workspace,
        l10n: Localizer,
        load_errors: Vec<(std::path::PathBuf, LoadError)>,
        restored: usize,
        session_file: Option<std::path::PathBuf>,
        settings: Settings,
        settings_dir: Option<std::path::PathBuf>,
    ) -> Self {
        let status = (restored > 0).then(|| (l10n.get("status-recovered"), false));
        let saved_macros = settings_dir
            .as_deref()
            .map(MacroStore::load)
            .unwrap_or_default();
        let mut shell = Self {
            ws,
            views: HashMap::new(),
            l10n,
            dialog: None,
            status,
            title: String::new(),
            allow_close: false,
            find: FindBar::default(),
            folder: Default::default(),
            syntax: SyntaxWorker::start(),
            spans: HashMap::new(),
            requested: HashMap::new(),
            counts: text3_app::counts::Counter::start(),
            counts_open: false,
            session_file,
            settings,
            settings_dir,
            settings_open: false,
            history: None,
            history_open: false,
            macro_cmds: Vec::new(),
            saved_macros,
            replace_preview: None,
            about_open: false,
            about_logo: None,
            headings: None,
            goto: Default::default(),
            default_apps: None,
            outline_open: false,
            lang_override: HashMap::new(),
            settings_tab: SettingsTab::General,
            settings_focus: false,
            shortcut_filter: String::new(),
            font_families: None,
            font_rx: None,
            font_wanted: None,
            allowed_image_dirs: Vec::new(),
            outline_follow: false,
            recent: Default::default(),
            tab_indent: HashMap::new(),
            split: None,
            pane_views: HashMap::new(),
            log: None,
            ctx: egui::Context::default(),
            pane_ax: None,
            refocus: false,
            update: None,
            recording: None,
            shortcut_moved: Vec::new(),
            shortcut_import: None,
            palette: Palette::default(),
            compare: None,
            encoding_pick: None,
            structured: Default::default(),
            structured_cache: HashMap::new(),
            csv_delimiter: HashMap::new(),
            csv_grids: HashMap::new(),
            trees: HashMap::new(),
            compare_pick: false,
            md_modes: HashMap::new(),
            md_cache: HashMap::new(),
            md_synced: HashMap::new(),
            md_anchor: HashMap::new(),
            images: Default::default(),
            applied_theme: None,
            themes: (Vec::new(), Vec::new()),
            quit_recorded: false,
            hot_exited: Vec::new(),
            owner: None,
            tab_in_view: None,
            focus: Default::default(),
            #[cfg(target_os = "macos")]
            native_menu: None,
            #[cfg(target_os = "macos")]
            menu_pending: false,
            bench: text3_app::benchprobe::config().map(bench::Driver::new),
        };
        // Files that are not UTF-8/UTF-16 go to the encoding picker; other
        // failures are reported together.
        let mut messages = Vec::new();
        for (path, e) in &load_errors {
            match e {
                LoadError::Decode { .. } if shell.encoding_pick.is_none() => {
                    shell.start_reopen(path)
                }
                _ => messages.push(describe_load_error(&shell.l10n, path, e)),
            }
        }
        if !messages.is_empty() {
            shell.dialog = Some(Dialog::Error(messages.join("\n")));
        }
        shell.focus_active();
        shell
    }

    fn t(&self, id: &str, args: &[(&str, FluentValue<'_>)]) -> String {
        tr(&self.l10n, id, args)
    }

    /// Labels of the editor's right-click menu.
    fn context_labels(&self) -> ContextLabels {
        ContextLabels {
            cut: self.t("edit-cut", &[]),
            copy: self.t("edit-copy", &[]),
            paste: self.t("edit-paste", &[]),
            select_all: self.t("edit-select-all", &[]),
        }
    }

    /// Move to the next or previous match and select it.
    fn find_step(&mut self, forward: bool) {
        let id = self.ws.active().id;
        self.find.refresh(id, &self.ws.active().editor, &self.ctx);
        let sel = self.editor().selections().primary().range();
        let from = if forward { sel.end } else { sel.start };
        if let Some(i) = self.find.matches.next_from(from, forward) {
            let r = self.find.matches.ranges[i].clone();
            self.editor_mut().select(r);
            self.active_view(id).reveal();
        }
    }

    fn find_event(&mut self, event: FindEvent) {
        let id = self.ws.active().id;
        match event {
            FindEvent::Next => self.find_step(true),
            FindEvent::Prev => self.find_step(false),
            FindEvent::Close => {
                self.find.open = false;
                self.focus_active();
            }
            FindEvent::Replace => {
                self.find.refresh(id, &self.ws.active().editor, &self.ctx);
                let with = self.find.replacement.clone();
                if let Some(m) = self.find.matcher().cloned() {
                    self.editor_mut().replace_selected(&m, &with);
                }
                self.find_step(true);
            }
            FindEvent::ReplaceAll => {
                self.find.refresh(id, &self.ws.active().editor, &self.ctx);
                let with = self.find.replacement.clone();
                // Show what would change; nothing is replaced until the
                // user confirms.
                if let Some(m) = self.find.matcher().cloned() {
                    let doc = self.editor().document();
                    let (tx, count) = m.replace_all(doc.rope(), &with);
                    if count == 0 {
                        let msg = self.t("status-replaced", &[("count", 0.into())]);
                        self.status = Some((msg, false));
                        return;
                    }
                    let lines =
                        text3_core::search::Matcher::preview(doc.rope(), &tx, PREVIEW_LINES);
                    self.replace_preview = Some(ReplacePreview {
                        tab: id,
                        revision: doc.revision(),
                        tx,
                        count,
                        lines,
                    });
                }
            }
        }
    }

    fn editor(&self) -> &Editor {
        &self.ws.active().editor
    }

    fn editor_mut(&mut self) -> &mut Editor {
        &mut self.ws.active_mut().editor
    }

    fn name_of(&self, ed: &Editor) -> String {
        ed.file_name().unwrap_or_else(|| self.t("untitled", &[]))
    }

    fn display_name(&self) -> String {
        self.name_of(self.editor())
    }

    fn focus_active(&mut self) {
        let id = self.ws.active().id;
        if self.pane_mode(id) == PaneContent::Structured {
            // The grid or tree takes the keys, as the editor would.
            let ed = &self.ws.active().editor;
            let target = match Self::data_kind(ed) {
                Some(DataKind::Csv | DataKind::Tsv) => grid_id(id),
                _ => tree_id(id),
            };
            self.ctx.memory_mut(|m| m.request_focus(target));
            return;
        }
        if self.pane_mode(id) == PaneContent::Preview {
            let slot = self.split.as_ref().map_or(0, |s| s.focused().slot);
            self.ctx
                .memory_mut(|m| m.request_focus(preview_focus_id(id, slot)));
            return;
        }
        self.active_view(id).request_focus();
    }

    /// The view showing the active tab: the focused pane's in a split.
    fn active_view(&mut self, id: TabId) -> &mut EditorView {
        let slot = self.split.as_ref().map_or(0, |s| s.focused().slot);
        self.slot_view(slot, id)
    }

    /// The display state of tab `id` in pane slot `slot`.
    fn slot_view(&mut self, slot: usize, id: TabId) -> &mut EditorView {
        if slot == 0 {
            self.views.entry(id).or_default()
        } else {
            self.pane_views.entry((slot, id)).or_default()
        }
    }

    fn save(&mut self) -> bool {
        let outcome = self.editor_mut().save();
        self.handle_save(outcome)
    }

    fn save_as(&mut self) -> bool {
        let mut dialog = rfd::FileDialog::new().set_file_name(self.display_name());
        if let Some(dir) = self.editor().file().and_then(|f| f.path.parent()) {
            dialog = dialog.set_directory(dir);
        }
        match dialog.save_file() {
            Some(path) => {
                let outcome = self.editor_mut().save_as(&path);
                self.handle_save(outcome)
            }
            None => false,
        }
    }

    fn handle_save(&mut self, outcome: SaveOutcome) -> bool {
        let name = self.display_name();
        match outcome {
            SaveOutcome::Saved => {
                self.record_history();
                if let Some(path) = self.editor().file().map(|f| f.path.clone()) {
                    self.note_recent(&path);
                }
                self.status = Some((self.t("status-saved", &[("name", name.into())]), false));
                return true;
            }
            SaveOutcome::NeedsPath => return self.save_as(),
            SaveOutcome::Conflict => self.dialog = Some(Dialog::Conflict),
            SaveOutcome::ReadOnly => self.dialog = Some(Dialog::ReadOnly),
            SaveOutcome::Failed(e) => {
                // Common causes get a plain explanation; the rest keep the
                // system's message. Either way nothing was overwritten.
                let kind = match &e {
                    text3_io::SaveError::Io { source, .. } => Some(source.kind()),
                    _ => None,
                };
                let msg = match kind {
                    Some(std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded) => {
                        self.t("error-save-disk-full", &[("name", name.into())])
                    }
                    Some(std::io::ErrorKind::PermissionDenied) => {
                        self.t("error-save-permission", &[("name", name.into())])
                    }
                    Some(std::io::ErrorKind::ReadOnlyFilesystem) => {
                        self.t("error-save-read-only-volume", &[("name", name.into())])
                    }
                    _ => {
                        let reason = match &e {
                            text3_io::SaveError::Io { source, .. } => source.to_string(),
                            other => other.to_string(),
                        };
                        self.t(
                            "error-save",
                            &[("name", name.into()), ("reason", reason.into())],
                        )
                    }
                };
                self.dialog = Some(Dialog::Error(msg));
            }
        }
        false
    }

    /// Close a tab, asking first if it has unsaved changes.
    fn request_close_tab(&mut self, id: TabId, ctx: &egui::Context) {
        let Some(i) = self.ws.position(id) else {
            return;
        };
        if self.ws.tabs()[i].editor.document().is_dirty() {
            self.ws.activate(i);
            self.dialog = Some(Dialog::Unsaved(Then::CloseTab(id)));
        } else {
            self.proceed(Then::CloseTab(id), ctx);
        }
    }

    /// Close every tab except `keep` (or all), leaving pinned tabs and tabs
    /// with unsaved changes open; the status bar says how many stayed.
    fn close_many(&mut self, keep: Option<TabId>, ctx: &egui::Context) {
        let ids: Vec<(TabId, bool)> = self
            .ws
            .tabs()
            .iter()
            .filter(|t| Some(t.id) != keep && !t.pinned)
            .map(|t| (t.id, t.editor.document().is_dirty()))
            .collect();
        let mut kept = 0i64;
        for (id, dirty) in ids {
            if dirty {
                kept += 1;
            } else {
                self.proceed(Then::CloseTab(id), ctx);
            }
        }
        if let Some(i) = keep.and_then(|id| self.ws.position(id)) {
            self.ws.activate(i);
        }
        self.status = (kept > 0).then(|| {
            (
                self.t("tabs-kept-unsaved", &[("count", kept.into())]),
                false,
            )
        });
        self.focus_active();
    }

    /// Put a tab's file path on the clipboard.
    fn copy_path(&mut self, id: TabId) {
        let Some(path) = self
            .ws
            .position(id)
            .and_then(|i| self.ws.tabs()[i].editor.file().map(|f| f.path.clone()))
        else {
            return;
        };
        let text = path.to_string_lossy().into_owned();
        if arboard::Clipboard::new()
            .and_then(|mut c| c.set_text(text))
            .is_ok()
        {
            self.status = Some((self.t("status-path-copied", &[]), false));
        }
    }

    /// Show a tab's file in the system file manager.
    fn reveal(&mut self, id: TabId) {
        let Some(path) = self
            .ws
            .position(id)
            .and_then(|i| self.ws.tabs()[i].editor.file().map(|f| f.path.clone()))
        else {
            return;
        };
        // System programs by full path: a bare name is looked up in the
        // folder of text3.exe first on Windows (planted explorer.exe).
        use text3_platform::{SystemDir, system_program};
        let missing = || std::io::Error::other("the system folder was not found");
        // macOS: ask Finder through NSWorkspace (also inside the App Store
        // sandbox, which cannot start programs); `open -R` as a fallback.
        if cfg!(target_os = "macos") && text3_platform::reveal_in_finder(&path) {
            return;
        }
        let spawned = if cfg!(target_os = "macos") {
            system_program("open", SystemDir::System)
                .ok_or_else(missing)
                .and_then(|open| {
                    std::process::Command::new(open)
                        .arg("-R")
                        .arg(&path)
                        .spawn()
                })
        } else if cfg!(windows) {
            let mut arg = std::ffi::OsString::from("/select,");
            arg.push(&path);
            system_program("explorer.exe", SystemDir::Windows)
                .ok_or_else(missing)
                .and_then(|explorer| std::process::Command::new(explorer).arg(arg).spawn())
        } else {
            let dir = path.parent().unwrap_or(&path).to_owned();
            std::process::Command::new("xdg-open").arg(dir).spawn()
        };
        if let Err(e) = spawned {
            self.status = Some((e.to_string(), true));
        }
    }

    /// Quit. With hot exit on, unsaved tabs are kept for the next start
    /// without asking; any that cannot be kept (and all of them with hot
    /// exit off) are asked about in turn.
    fn request_quit(&mut self, ctx: &egui::Context) {
        // Keep unsaved tabs on every step of the flow (a tab already on
        // disk at its revision is not written again). The session is
        // recorded once per attempt, before the flow starts closing tabs;
        // an attempt that ends without quitting is reset in `ui`.
        if self.settings.hot_exit {
            self.keep_unsaved();
        }
        if !std::mem::replace(&mut self.quit_recorded, true) {
            self.save_session();
        }
        let ask = self.ws.first_unkept(&self.hot_exited);
        match ask {
            Some(i) => {
                self.ws.activate(i);
                self.dialog = Some(Dialog::Unsaved(Then::Quit));
            }
            None => {
                self.ws.flush();
                self.allow_close = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
        }
    }

    /// Hot exit: write every unsaved tab to the recovery folder now. Tabs
    /// that could not be written are left to the quit prompts, and the
    /// status bar says why.
    fn keep_unsaved(&mut self) {
        let result = self
            .ws
            .hot_exit(text3_io::recovery::MAX_TEXT, HOT_EXIT_WAIT);
        self.hot_exited = result.kept;
        let name_of = |id: TabId| {
            self.ws
                .position(id)
                .map(|i| self.name_of(&self.ws.tabs()[i].editor))
                .unwrap_or_default()
        };
        let too_large = result
            .failed
            .iter()
            .find(|(_, e)| *e == text3_app::recovery::KeepError::TooLarge);
        let msg = match (too_large, result.failed.first()) {
            (Some((id, _)), _) => Some(self.t(
                "status-hot-exit-too-large",
                &[("name", name_of(*id).into())],
            )),
            (None, Some((_, text3_app::recovery::KeepError::Failed(reason)))) => Some(self.t(
                "status-hot-exit-failed",
                &[("reason", reason.clone().into())],
            )),
            _ => None,
        };
        if let Some(msg) = msg {
            self.status = Some((msg, true));
        }
    }

    /// Write the session file, with the tabs hot exit kept.
    fn save_session(&self) {
        if let Some(f) = &self.session_file
            && let Err(e) = self
                .ws
                .session_with(self.split.as_ref(), &self.hot_exited)
                .save(f)
        {
            eprintln!("text3: could not save session: {e}");
        }
    }

    /// The process is ending without the quit flow having finished (the
    /// system is logging out or shutting down, or the app was quit from
    /// outside, e.g. the Dock): nothing can be asked any more. Every
    /// unsaved tab is written to the recovery folder, so it comes back at
    /// the next start; with hot exit on, the session records them too.
    fn exiting(&mut self) {
        // Also after a finished quit flow, as a backstop: tabs already on
        // disk at their revision are not written again.
        let result = self
            .ws
            .hot_exit(text3_io::recovery::MAX_TEXT, HOT_EXIT_WAIT);
        for (id, e) in &result.failed {
            eprintln!("text3: unsaved tab {id} could not be kept: {e:?}");
        }
        if !self.allow_close {
            self.hot_exited = if self.settings.hot_exit {
                result.kept
            } else {
                Vec::new()
            };
            self.save_session();
        }
        self.ws.flush();
        // Other instances may restore what is left once this one is gone.
        drop(self.owner.take());
    }

    /// Hold the recovery folder's owner lock until the app exits.
    pub fn set_owner_lock(&mut self, lock: Option<text3_io::recovery::OwnerLock>) {
        self.owner = lock;
    }

    /// Tell the user about restored tabs whose file changed on disk or is
    /// gone. A changed file opens the external-change dialog for its tab:
    /// the recovered text stays in the tab and the file is not touched
    /// until the user chooses.
    pub fn show_restore_notes(&mut self) {
        let mut messages = Vec::new();
        for (id, state) in self.ws.take_restore_notes() {
            let Some(i) = self.ws.position(id) else {
                continue;
            };
            let name = self.name_of(&self.ws.tabs()[i].editor);
            match state {
                Restored::ChangedOnDisk => {
                    messages.push(self.t("status-restored-changed", &[("name", name.into())]));
                    if self.dialog.is_none() {
                        self.ws.activate(i);
                        self.dialog = Some(Dialog::Conflict);
                    }
                }
                Restored::Missing => {
                    messages.push(self.t("status-restored-missing", &[("name", name.into())]))
                }
                Restored::Applied => {}
            }
        }
        if !messages.is_empty() {
            self.status = Some((messages.join(" "), true));
        }
    }

    /// Scroll each tab to its restored selection when it is first shown.
    pub fn reveal_restored_carets(&mut self) {
        let ids: Vec<TabId> = self
            .ws
            .tabs()
            .iter()
            .filter(|t| t.editor.selections().primary().head > 0)
            .map(|t| t.id)
            .collect();
        for id in ids {
            self.views.entry(id).or_default().reveal();
        }
    }

    /// Continue after the active tab was saved or its changes declined.
    fn proceed(&mut self, then: Then, ctx: &egui::Context) {
        match then {
            Then::CloseTab(id) => {
                if let Some(i) = self.ws.position(id) {
                    self.ws.close(i);
                    self.views.remove(&id);
                    self.pane_views.retain(|(_, t), _| *t != id);
                    self.spans.remove(&id);
                    self.requested.remove(&id);
                    self.syntax.forget(id);
                    self.counts.forget(id);
                    self.md_cache.remove(&id);
                    self.md_modes.remove(&id);
                    self.md_synced.retain(|(t, _), _| *t != id);
                    self.md_anchor.remove(&id);
                    self.structured.remove(&id);
                    self.structured_cache.remove(&id);
                    self.csv_delimiter.remove(&id);
                    self.csv_grids.remove(&id);
                    self.trees.remove(&id);
                }
            }
            Then::Quit => {
                let id = self.ws.active().id;
                let i = self.ws.active_index();
                self.ws.close(i);
                self.views.remove(&id);
                self.request_quit(ctx);
                return;
            }
        }
        self.status = None;
        self.focus_active();
    }

    fn open_dialog(&mut self) {
        let Some(paths) = rfd::FileDialog::new().pick_files() else {
            return;
        };
        for p in paths {
            self.open(&p);
        }
    }

    /// Files at least this large open in the log viewer instead of an
    /// editor tab (editing loads the whole file into memory).
    const LARGE_FILE: u64 = 256 * 1024 * 1024;

    fn open_log(&mut self, path: &Path, ctx: &egui::Context) {
        match crate::logview::LogView::open(path, ctx) {
            Ok(v) => {
                self.log = Some(v);
                self.status = None;
            }
            Err(e) => {
                self.dialog = Some(Dialog::Error(describe_load_error(
                    &self.l10n,
                    path,
                    &LoadError::Io {
                        path: path.to_owned(),
                        source: e,
                    },
                )))
            }
        }
    }

    /// Whether `path` is big enough to open in the log viewer.
    pub fn is_large(path: &Path) -> bool {
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() >= Self::LARGE_FILE)
    }

    /// Open a large file in the log viewer and say why.
    pub fn open_large(&mut self, path: &Path, ctx: &egui::Context) {
        self.open_log(path, ctx);
        if self.log.is_some() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.status = Some((self.t("log-opened-large", &[("name", name.into())]), false));
        }
    }

    fn open(&mut self, path: &Path) {
        if Self::is_large(path) {
            let ctx = self.ctx.clone();
            self.open_large(path, &ctx);
            return;
        }
        match self.ws.open(path) {
            Ok(_) => {
                self.note_recent(path);
                self.status = None;
                self.focus_active();
            }
            // Not valid UTF-8/UTF-16: let the user pick an encoding instead
            // of failing.
            Err(LoadError::Decode { .. }) => self.start_reopen(path),
            Err(e) => self.dialog = Some(Dialog::Error(describe_load_error(&self.l10n, path, &e))),
        }
    }

    fn save_settings(&mut self) {
        if let Some(dir) = &self.settings_dir
            && let Err(e) = self.settings.save(dir)
        {
            let msg = self.t("settings-save-failed", &[("reason", e.to_string().into())]);
            self.status = Some((msg, true));
        }
    }

    fn apply_theme(&mut self, ctx: &egui::Context) {
        let key = (self.settings.theme, self.settings.color_theme.clone());
        if self.applied_theme.as_ref() == Some(&key) {
            return;
        }
        if self.themes.0.is_empty() {
            self.reload_themes();
        }
        let chosen = self.settings.color_theme.as_ref().and_then(|id| {
            self.themes
                .0
                .iter()
                .find(|t| &t.id == id)
                .map(|t| t.palette)
        });
        if let Some(p) = chosen {
            theme::set_override(ctx, Some(p));
        } else {
            theme::set_override(ctx, None);
            theme::set_high_contrast(ctx, self.settings.theme == ThemeChoice::HighContrast);
            ctx.set_theme(match self.settings.theme {
                ThemeChoice::System => egui::ThemePreference::System,
                ThemeChoice::Light => egui::ThemePreference::Light,
                ThemeChoice::Dark | ThemeChoice::HighContrast => egui::ThemePreference::Dark,
            });
        }
        self.applied_theme = Some(key);
    }

    /// Read the bundled themes and the user's `.theme` files again.
    fn reload_themes(&mut self) {
        self.themes = crate::themes::all(self.settings_dir.as_deref());
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let before = self.settings.clone();
        if self.settings_tab == SettingsTab::Appearance && self.themes.0.is_empty() {
            self.reload_themes();
        }
        let theme_list = self.themes.0.clone();
        let theme_errors = self.themes.1.clone();
        let user_theme_note = self.t(
            "settings-theme-user-note",
            &[(
                "folder",
                self.settings_dir
                    .as_ref()
                    .map(|d| d.join("themes").display().to_string())
                    .unwrap_or_default()
                    .into(),
            )],
        );
        let mut open = true;
        let title = self.t("settings-title", &[]);
        let l = |id: &str| tr(&self.l10n, id, &[]);
        let s = &mut self.settings;
        let recording = &mut self.recording;
        let moved = &mut self.shortcut_moved;
        let import_note = &mut self.shortcut_import;
        let l10n = &self.l10n;
        let tab = &mut self.settings_tab;
        let filter = &mut self.shortcut_filter;
        let families = self.font_families.clone();
        let scanning = self.font_rx.is_some();
        let mut scan_fonts = false;
        let mut open_default_apps = false;
        let was_recording = recording.is_some();
        let mut focus_search = false;
        let focus_tab = std::mem::take(&mut self.settings_focus);
        egui::Window::new(title)
            .id(egui::Id::unique("text3-settings"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([680.0, 480.0])
            .min_size([520.0, 320.0])
            .show(ctx, |ui| {
                ui.horizontal_top(|ui| {
                    // Section list: a tab list. Only the selected tab is a
                    // Tab stop; arrow keys move between the tabs, Tab goes on
                    // into the section.
                    const SECTIONS: [(SettingsTab, &str); 4] = [
                        (SettingsTab::General, "settings-tab-general"),
                        (SettingsTab::Editor, "settings-tab-editor"),
                        (SettingsTab::Appearance, "settings-tab-appearance"),
                        (SettingsTab::Shortcuts, "settings-shortcuts"),
                    ];
                    ui.vertical(|ui| {
                        ui.set_width(150.0);
                        ui.ctx().accesskit_node_builder(ui.unique_id(), |n| {
                            n.set_role(egui::accesskit::Role::TabList);
                            n.set_label(l("a11y-settings-sections"));
                        });
                        let mut ids = [egui::Id::NULL; SECTIONS.len()];
                        let mut focused = None;
                        for (k, (t, id)) in SECTIONS.into_iter().enumerate() {
                            let selected = *tab == t;
                            let sense = if selected {
                                egui::Sense::click()
                            } else {
                                egui::Sense::CLICK
                            };
                            let r = ui.add_sized(
                                [150.0, 26.0],
                                egui::Button::selectable(selected, l(id)).sense(sense),
                            );
                            ui.ctx().accesskit_node_builder(r.id, |n| {
                                n.set_role(egui::accesskit::Role::Tab);
                                n.clear_toggled();
                                n.set_selected(selected);
                            });
                            ids[k] = r.id;
                            if r.has_focus() {
                                focused = Some(k);
                            }
                            if selected && focus_tab {
                                ui.memory_mut(|m| m.request_focus(r.id));
                            }
                            if r.clicked() {
                                if t == SettingsTab::Shortcuts && *tab != t {
                                    focus_search = true;
                                }
                                *tab = t;
                            }
                        }
                        if let Some(k) = focused {
                            ui.memory_mut(|m| {
                                m.set_focus_lock_filter(
                                    ids[k],
                                    egui::EventFilter {
                                        horizontal_arrows: true,
                                        vertical_arrows: true,
                                        ..Default::default()
                                    },
                                );
                            });
                            let n = SECTIONS.len();
                            let to = ui.input_mut(|i| {
                                use egui::{Key, Modifiers as M};
                                if i.consume_key(M::NONE, Key::ArrowDown)
                                    || i.consume_key(M::NONE, Key::ArrowRight)
                                {
                                    Some((k + 1) % n)
                                } else if i.consume_key(M::NONE, Key::ArrowUp)
                                    || i.consume_key(M::NONE, Key::ArrowLeft)
                                {
                                    Some((k + n - 1) % n)
                                } else if i.consume_key(M::NONE, Key::Home) {
                                    Some(0)
                                } else if i.consume_key(M::NONE, Key::End) {
                                    Some(n - 1)
                                } else {
                                    None
                                }
                            });
                            if let Some(j) = to {
                                *tab = SECTIONS[j].0;
                                ui.memory_mut(|m| m.request_focus(ids[j]));
                            }
                        }
                    });
                    ui.separator();
                    let section = SECTIONS
                        .iter()
                        .find(|(t, _)| *t == *tab)
                        .map_or(String::new(), |(_, id)| l(id));
                    ui.vertical(|ui| {
                        ui.ctx().accesskit_node_builder(ui.unique_id(), |n| {
                            n.set_role(egui::accesskit::Role::TabPanel);
                            n.set_label(section);
                        });
                        ui.set_min_height(ui.available_height());
                        ui.set_min_width(480.0);
                        match *tab {
                            SettingsTab::General => {
                                egui::Grid::new("settings-general")
                                    .num_columns(2)
                                    .spacing([16.0, 10.0])
                                    .show(ui, |ui| {
                                        ui.label(l("settings-language"));
                                        let current = s
                                            .locale
                                            .as_deref()
                                            .and_then(|t| {
                                                text3_i18n::LANGUAGES
                                                    .iter()
                                                    .find(|(tag, _)| *tag == t)
                                            })
                                            .map_or_else(
                                                || l("settings-language-system"),
                                                |(_, name)| (*name).to_owned(),
                                            );
                                        egui::ComboBox::from_id_salt("settings-language")
                                            .selected_text(current)
                                            .show_ui(ui, |ui| {
                                                ui.selectable_value(
                                                    &mut s.locale,
                                                    None,
                                                    l("settings-language-system"),
                                                );
                                                // Language names in their own language.
                                                for (tag, name) in text3_i18n::LANGUAGES {
                                                    ui.selectable_value(
                                                        &mut s.locale,
                                                        Some((*tag).to_owned()),
                                                        *name,
                                                    );
                                                }
                                            });
                                        ui.end_row();
                                        ui.label("");
                                        ui.weak(l("settings-language-note"));
                                        ui.end_row();
                                        ui.label("");
                                        ui.checkbox(
                                            &mut s.restore_session,
                                            l("settings-restore-session"),
                                        );
                                        ui.end_row();
                                        ui.label("");
                                        ui.checkbox(&mut s.hot_exit, l("settings-hot-exit"))
                                            .on_hover_text(l("settings-hot-exit-detail"));
                                        ui.end_row();
                                        ui.label("");
                                        ui.checkbox(
                                            &mut s.local_history,
                                            l("settings-local-history"),
                                        )
                                        .on_hover_text(l("settings-local-history-detail"));
                                        ui.end_row();
                                        // The App Store build: updates come from the
                                        // store; the sandbox cannot change defaults.
                                        if !text3_app::STORE {
                                            ui.label("");
                                            ui.checkbox(
                                                &mut s.check_updates,
                                                l("settings-check-updates"),
                                            )
                                            .on_hover_text(l("settings-check-updates-detail"));
                                            ui.end_row();
                                            ui.label(l("settings-default-apps"));
                                            if ui
                                                .button(l("settings-default-apps-button"))
                                                .clicked()
                                            {
                                                open_default_apps = true;
                                            }
                                            ui.end_row();
                                        }
                                    });
                            }
                            SettingsTab::Editor => {
                                egui::Grid::new("settings-editor")
                                    .num_columns(2)
                                    .spacing([16.0, 10.0])
                                    .show(ui, |ui| {
                                        ui.label(l("settings-font"));
                                        let default_name = l("settings-font-default");
                                        let current = s
                                            .editor_font
                                            .clone()
                                            .unwrap_or_else(|| default_name.clone());
                                        match &families {
                                            None => {
                                                scan_fonts = !scanning;
                                                ui.horizontal(|ui| {
                                                    ui.spinner();
                                                    ui.weak(l("settings-font-loading"));
                                                });
                                            }
                                            Some(list) => {
                                                egui::ComboBox::from_id_salt("settings-font")
                                                    .selected_text(current)
                                                    .width(240.0)
                                                    .height(320.0)
                                                    .show_ui(ui, |ui| {
                                                        ui.selectable_value(
                                                            &mut s.editor_font,
                                                            None,
                                                            default_name,
                                                        );
                                                        // Popular fonts first (#176).
                                                        for (i, group) in
                                                            [&list.recommended, &list.others]
                                                                .into_iter()
                                                                .enumerate()
                                                        {
                                                            if i > 0 && !group.is_empty() {
                                                                ui.separator();
                                                            }
                                                            for f in group {
                                                                ui.selectable_value(
                                                                    &mut s.editor_font,
                                                                    Some(f.clone()),
                                                                    f,
                                                                );
                                                            }
                                                        }
                                                    });
                                            }
                                        }
                                        ui.end_row();
                                        ui.label("");
                                        ui.label(
                                            egui::RichText::new(
                                                "fn main() { let 0O = 1lI; } // äö → ≠",
                                            )
                                            .font(egui::FontId::monospace(s.font_size)),
                                        );
                                        ui.end_row();
                                        ui.label(l("settings-font-size"));
                                        ui.add(
                                            egui::DragValue::new(&mut s.font_size)
                                                .range(
                                                    text3_app::settings::FONT_SIZE_MIN
                                                        ..=text3_app::settings::FONT_SIZE_MAX,
                                                )
                                                .speed(0.25)
                                                .suffix(" pt"),
                                        );
                                        ui.end_row();
                                        ui.label(l("settings-tab-width"));
                                        ui.add(
                                            egui::DragValue::new(&mut s.tab_width).range(1..=16),
                                        );
                                        ui.end_row();
                                        ui.label("");
                                        ui.checkbox(
                                            &mut s.insert_spaces,
                                            l("settings-insert-spaces"),
                                        );
                                        ui.end_row();
                                        ui.label("");
                                        ui.checkbox(&mut s.word_wrap, l("settings-word-wrap"));
                                        ui.end_row();
                                        ui.label("");
                                        ui.checkbox(&mut s.show_counts, l("settings-show-counts"));
                                        ui.end_row();
                                    });
                            }
                            SettingsTab::Appearance => {
                                ui.label(l("settings-theme"));
                                ui.add_space(4.0);
                                for (v, id) in [
                                    (ThemeChoice::System, "settings-theme-system"),
                                    (ThemeChoice::Light, "settings-theme-light"),
                                    (ThemeChoice::Dark, "settings-theme-dark"),
                                    (ThemeChoice::HighContrast, "settings-theme-high-contrast"),
                                ] {
                                    if ui
                                        .radio(s.color_theme.is_none() && s.theme == v, l(id))
                                        .clicked()
                                    {
                                        s.theme = v;
                                        s.color_theme = None;
                                    }
                                }
                                ui.add_space(10.0);
                                ui.strong(l("settings-theme-more"));
                                egui::ScrollArea::vertical()
                                    .id_salt("settings-themes")
                                    .max_height(240.0)
                                    .show(ui, |ui| {
                                        for t in &theme_list {
                                            ui.horizontal(|ui| {
                                                let sel = s.color_theme.as_deref() == Some(&t.id);
                                                let (rect, _) = ui.allocate_exact_size(
                                                    egui::vec2(54.0, 16.0),
                                                    egui::Sense::hover(),
                                                );
                                                let p = t.palette;
                                                let painter = ui.painter();
                                                painter.rect_filled(rect, 3.0, p.canvas);
                                                for (i, c) in [
                                                    p.text,
                                                    p.syntax.keyword,
                                                    p.syntax.string,
                                                    p.syntax.function,
                                                ]
                                                .into_iter()
                                                .enumerate()
                                                {
                                                    let x = rect.left() + 6.0 + i as f32 * 12.0;
                                                    painter.circle_filled(
                                                        egui::pos2(x, rect.center().y),
                                                        4.0,
                                                        c,
                                                    );
                                                }
                                                if ui.radio(sel, &t.name).clicked() {
                                                    s.color_theme = Some(t.id.clone());
                                                }
                                            });
                                        }
                                    });
                                ui.add_space(6.0);
                                ui.weak(user_theme_note.clone());
                                for (file, err) in &theme_errors {
                                    ui.colored_label(
                                        theme::palette(ui.ctx()).warning,
                                        format!("{file}: {err}"),
                                    );
                                }
                            }
                            SettingsTab::Shortcuts => {
                                ui.horizontal(|ui| {
                                    ui.label(l("settings-keymap-preset"));
                                    use text3_app::keymap::Preset;
                                    for (p, id) in [
                                        (Preset::Default, "settings-keymap-default"),
                                        (Preset::NotepadPlusPlus, "settings-keymap-notepadpp"),
                                    ] {
                                        ui.radio_value(&mut s.keymap.preset, p, l(id));
                                    }
                                });
                                ui.horizontal_wrapped(|ui| {
                                    if ui
                                        .button(l("settings-shortcut-import-npp"))
                                        .on_hover_text(l("settings-shortcut-import-npp-hint"))
                                        .clicked()
                                        && let Some(path) = rfd::FileDialog::new()
                                            .add_filter("XML", &["xml"])
                                            .pick_file()
                                    {
                                        *import_note =
                                            Some(import_npp_shortcuts(&path, &mut s.keymap, l10n));
                                        moved.clear();
                                    }
                                    if let Some((text, error)) = import_note.as_ref() {
                                        let p = theme::palette(ui.ctx());
                                        let r = ui.colored_label(
                                            if *error { p.warning } else { p.text },
                                            text,
                                        );
                                        crate::structured::announce(ui, r.id);
                                    }
                                });
                                let search = ui
                                    .add(
                                        egui::TextEdit::singleline(filter)
                                            .hint_text(l("settings-shortcut-search"))
                                            .desired_width(f32::INFINITY),
                                    )
                                    .accessible_name(l("settings-shortcut-search"));
                                if focus_search {
                                    search.request_focus();
                                }
                                if !moved.is_empty() {
                                    let r = ui.colored_label(
                                        theme::palette(ui.ctx()).warning,
                                        format!(
                                            "{} {}",
                                            l("settings-shortcut-moved"),
                                            moved.join(", ")
                                        ),
                                    );
                                    crate::structured::announce(ui, r.id);
                                }
                                let needle = filter.to_lowercase();
                                egui::ScrollArea::vertical()
                                    .auto_shrink(false)
                                    .show(ui, |ui| {
                                        // Grouped by menu, in menu order.
                                        const GROUPS: [&str; 6] =
                                            ["file", "edit", "view", "tools", "window", "app"];
                                        for g in GROUPS {
                                            let rows: Vec<(MenuAction, String, String)> =
                                                MenuAction::ALL
                                                    .into_iter()
                                                    .filter(|a| {
                                                        a.command_id().split('.').next() == Some(g)
                                                    })
                                                    .map(|a| {
                                                        let label = l(a.label_id())
                                                            .trim_end_matches('…')
                                                            .to_owned();
                                                        let keys = a
                                                            .shortcut(&s.keymap)
                                                            .map(|k| ctx.format_shortcut(&k))
                                                            .unwrap_or_default();
                                                        (a, label, keys)
                                                    })
                                                    .filter(|(_, label, keys)| {
                                                        needle.is_empty()
                                                            || label
                                                                .to_lowercase()
                                                                .contains(&needle)
                                                            || keys.to_lowercase().contains(&needle)
                                                    })
                                                    .collect();
                                            if rows.is_empty() {
                                                continue;
                                            }
                                            ui.add_space(6.0);
                                            ui.strong(match g {
                                                "app" => "Text³".to_owned(),
                                                other => l(&format!("menu-{other}")),
                                            });
                                            egui::Grid::new(("shortcuts", g))
                                                .num_columns(3)
                                                .striped(true)
                                                .min_col_width(120.0)
                                                .spacing([16.0, 4.0])
                                                .show(ui, |ui| {
                                                    for (a, label, keys) in rows {
                                                        ui.label(&label);
                                                        let waiting = *recording == Some(a);
                                                        let text = if waiting {
                                                            l("settings-shortcut-press")
                                                        } else {
                                                            keys.clone()
                                                        };
                                                        ui.monospace(text);
                                                        // The buttons name their command for
                                                        // screen readers; while waiting for a
                                                        // chord, Change says so.
                                                        let lt = |id: &str| {
                                                            tr(
                                                                l10n,
                                                                id,
                                                                &[(
                                                                    "command",
                                                                    label.clone().into(),
                                                                )],
                                                            )
                                                        };
                                                        let change_name = if waiting {
                                                            l("settings-shortcut-press")
                                                        } else if keys.is_empty() {
                                                            lt("a11y-shortcut-change")
                                                        } else {
                                                            tr(
                                                                l10n,
                                                                "a11y-shortcut-change-current",
                                                                &[
                                                                    (
                                                                        "command",
                                                                        label.clone().into(),
                                                                    ),
                                                                    ("keys", keys.clone().into()),
                                                                ],
                                                            )
                                                        };
                                                        ui.horizontal(|ui| {
                                                            if ui
                                                                .small_button(l(
                                                                    "settings-shortcut-change",
                                                                ))
                                                                .accessible_name(change_name)
                                                                .clicked()
                                                            {
                                                                *recording = Some(a);
                                                            }
                                                            if s.keymap
                                                                .custom
                                                                .contains_key(a.command_id())
                                                                && ui
                                                                    .small_button(l(
                                                                        "settings-shortcut-reset",
                                                                    ))
                                                                    .accessible_name(lt(
                                                                        "a11y-shortcut-reset",
                                                                    ))
                                                                    .clicked()
                                                            {
                                                                s.keymap
                                                                    .custom
                                                                    .remove(a.command_id());
                                                            }
                                                        });
                                                        ui.end_row();
                                                    }
                                                });
                                        }
                                    });
                            }
                        }
                    });
                });
                if let Some(a) = *recording {
                    // Capture the next chord; Escape cancels.
                    let pressed = ui.input(|i| {
                        i.events.iter().find_map(|e| match e {
                            egui::Event::Key {
                                key,
                                pressed: true,
                                modifiers,
                                ..
                            } => Some((*key, *modifiers)),
                            _ => None,
                        })
                    });
                    match pressed {
                        Some((egui::Key::Escape, _)) => *recording = None,
                        Some((key, m)) => {
                            if let Some(chord) = menu::chord_from_press(key, m) {
                                let clash: Vec<String> = MenuAction::ALL
                                    .into_iter()
                                    .filter(|o| {
                                        *o != a
                                            && o.shortcut(&s.keymap) == menu::chord_to_egui(&chord)
                                    })
                                    .map(|o| l(o.label_id()))
                                    .collect();
                                // The other command loses this shortcut.
                                for o in MenuAction::ALL {
                                    if o != a
                                        && o.shortcut(&s.keymap) == menu::chord_to_egui(&chord)
                                    {
                                        s.keymap.custom.insert(o.command_id().to_owned(), None);
                                    }
                                }
                                s.keymap
                                    .custom
                                    .insert(a.command_id().to_owned(), Some(chord));
                                *recording = None;
                                *moved = clash;
                            }
                        }
                        None => {}
                    }
                }
            });
        if open_default_apps {
            self.open_default_apps();
        }
        if scan_fonts {
            self.font_job(|| FontMsg::Families(text3_app::fonts::monospace_families()));
        }
        if self.settings.editor_font != before.editor_font {
            self.request_editor_font();
        }
        if self.settings != before {
            self.save_settings();
        }
        // Keep the native menu's shortcuts in step; clear them while
        // recording so the menu does not swallow the key press.
        #[cfg(target_os = "macos")]
        if let Some(m) = &self.native_menu {
            if self.recording.is_some() && !was_recording {
                m.clear_shortcuts();
            } else if (was_recording && self.recording.is_none())
                || self.settings.keymap != before.keymap
            {
                m.apply_keymap(&self.settings.keymap);
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = was_recording;
        if !open {
            self.settings_open = false;
            self.focus_active();
        }
    }

    /// Run a text tool on the selection (or whole document) as one edit.
    fn run_tool(&mut self, action: MenuAction) {
        use text3_tools::{Newline, json};
        let indent = self.settings.indent();
        let newline = match self.editor().line_ending() {
            LineEnding::CrLf => "\r\n",
            LineEnding::Cr => "\r",
            _ => "\n",
        };
        let ok = |s: String| Ok::<_, json::JsonError>(s);
        let result = match action {
            MenuAction::JsonValidate => {
                let range = self.editor().tool_range();
                let text: String = self.editor().document().rope().slice(range.clone()).into();
                json::validate_json(&text).map(|()| false)
            }
            MenuAction::JsonFormat => self
                .editor_mut()
                .transform(|t| json::format(t, &indent, newline)),
            MenuAction::JsonMinify => self.editor_mut().transform(json::minify),
            MenuAction::SortLines => self
                .editor_mut()
                .transform(|t| ok(text3_tools::sort_lines(t, false))),
            MenuAction::SortLinesDesc => self
                .editor_mut()
                .transform(|t| ok(text3_tools::sort_lines(t, true))),
            MenuAction::UniqueLines => self
                .editor_mut()
                .transform(|t| ok(text3_tools::unique_lines(t))),
            MenuAction::TrimTrailing => self
                .editor_mut()
                .transform(|t| ok(text3_tools::trim_trailing_whitespace(t))),
            MenuAction::UpperCase => self
                .editor_mut()
                .transform(|t| ok(text3_tools::upper_case(t))),
            MenuAction::LowerCase => self
                .editor_mut()
                .transform(|t| ok(text3_tools::lower_case(t))),
            MenuAction::ToLf => self
                .editor_mut()
                .transform(|t| ok(text3_tools::convert_newlines(t, Newline::Lf))),
            MenuAction::ToCrLf => self
                .editor_mut()
                .transform(|t| ok(text3_tools::convert_newlines(t, Newline::CrLf))),
            _ => return,
        };
        match result {
            Ok(changed) => {
                let id = match (action, changed) {
                    (MenuAction::JsonValidate, _) => "tools-json-valid",
                    (_, true) => "tools-done",
                    (_, false) => "tools-no-change",
                };
                self.status = Some((self.t(id, &[]), false));
            }
            Err(e) => {
                // Put the caret on the problem.
                let range = self.editor().tool_range();
                let text: String = self.editor().document().rope().slice(range.clone()).into();
                let at = range.start + text[..e.offset.min(text.len())].chars().count();
                self.editor_mut().select(at..at);
                let id = self.ws.active().id;
                self.active_view(id).reveal();
                let msg = self.t(
                    "tools-json-error",
                    &[
                        ("line", (e.line as i64).into()),
                        ("column", (e.column as i64).into()),
                        ("reason", self.t(&json_reason_id(e.message), &[]).into()),
                    ],
                );
                self.status = Some((msg, true));
            }
        }
    }

    /// A file that is not UTF-8: open UTF-16 without a BOM directly (it
    /// round-trips exactly), otherwise ask, suggesting a guessed encoding.
    fn start_reopen(&mut self, path: &std::path::Path) {
        use std::io::Read;
        const GUESS_LIMIT: u64 = 16 * 1024 * 1024;
        let mut bytes = Vec::new();
        let read =
            std::fs::File::open(path).and_then(|f| f.take(GUESS_LIMIT + 1).read_to_end(&mut bytes));
        if let Err(e) = read {
            self.dialog = Some(Dialog::Error(e.to_string()));
            return;
        }
        // Larger files: guess from the start; opening still checks that
        // the whole file decodes and round-trips exactly.
        let suggested = if bytes.len() as u64 <= GUESS_LIMIT {
            text3_io::guess(&bytes)
        } else {
            text3_io::guess_head(&bytes[..64 * 1024])
        };
        if let Some(enc) = suggested.filter(|e| text3_io::is_confident(*e))
            && self.ws.open_as(path, enc).is_ok()
        {
            let msg = self.t("encoding-detected", &[("encoding", enc.name().into())]);
            self.status = Some((msg, false));
            self.focus_active();
            return;
        }
        let choices = encoding_choices(true);
        // Windows-1252, the most common legacy case, unless guessed.
        let default = choices
            .iter()
            .position(|e| *e == TextEncoding::Legacy(text3_io::LEGACY[0]))
            .unwrap_or(0);
        let selected = suggested
            .and_then(|s| choices.iter().position(|e| *e == s))
            .unwrap_or(default);
        bytes.truncate(64 * 1024);
        self.encoding_pick = Some(EncodingPick::Reopen {
            path: path.to_owned(),
            head: bytes,
            selected,
            error: None,
            suggested,
        });
    }

    fn encoding_label(&self, e: TextEncoding) -> String {
        match e {
            TextEncoding::Legacy(enc) => {
                let id = format!("enc-{}", enc.name().to_ascii_lowercase());
                format!("{} ({})", self.t(&id, &[]), enc.name())
            }
            other => other.name().to_owned(),
        }
    }

    fn encoding_window(&mut self, ctx: &egui::Context) {
        let Some(pick) = &self.encoding_pick else {
            return;
        };
        let reopen = matches!(pick, EncodingPick::Reopen { .. });
        let choices = encoding_choices(reopen);
        let labels: Vec<String> = choices.iter().map(|e| self.encoding_label(*e)).collect();
        let title = if reopen {
            self.t("encoding-reopen-title", &[])
        } else {
            self.t("encoding-save-title", &[])
        };
        let (ok_label, cancel_label) = (
            self.t(if reopen { "button-open" } else { "button-save" }, &[]),
            self.t("button-cancel", &[]),
        );
        let p = theme::palette(ctx);
        let mut open = true;
        let mut confirm = false;
        let mut cancel = false;
        let Some(pick) = &self.encoding_pick else {
            return;
        };
        let preview = match pick {
            EncodingPick::Reopen { head, selected, .. } => {
                let enc = choices
                    .get(*selected)
                    .copied()
                    .unwrap_or(TextEncoding::Utf8);
                let text = match enc {
                    TextEncoding::Legacy(e) => e.decode_without_bom_handling(head).0.into_owned(),
                    TextEncoding::Utf16Le | TextEncoding::Utf16Be => {
                        let le = enc == TextEncoding::Utf16Le;
                        let units: Vec<u16> = head
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|c| {
                                if le {
                                    u16::from_le_bytes(*c)
                                } else {
                                    u16::from_be_bytes(*c)
                                }
                            })
                            .collect();
                        String::from_utf16_lossy(&units)
                    }
                    _ => String::from_utf8_lossy(head).into_owned(),
                };
                Some(text.chars().take(1200).collect::<String>())
            }
            EncodingPick::Save { .. } => None,
        };
        let suggestion = match pick {
            EncodingPick::Reopen {
                suggested: Some(e), ..
            } => Some(self.t(
                "encoding-suggested",
                &[("encoding", self.encoding_label(*e).into())],
            )),
            _ => None,
        };
        let (selected, error) = match &mut self.encoding_pick {
            Some(
                EncodingPick::Reopen {
                    selected, error, ..
                }
                | EncodingPick::Save { selected, error },
            ) => (selected, error),
            None => return,
        };
        egui::Window::new(title)
            .id(egui::Id::unique("text3-encoding"))
            .open(&mut open)
            .collapsible(false)
            .default_width(520.0)
            .show(ctx, |ui| {
                if let Some(s) = &suggestion {
                    ui.colored_label(p.muted, s);
                    ui.add_space(4.0);
                }
                egui::ComboBox::from_id_salt("encoding-choice")
                    .width(360.0)
                    .selected_text(labels.get(*selected).cloned().unwrap_or_default())
                    .show_ui(ui, |ui| {
                        for (i, l) in labels.iter().enumerate() {
                            if ui.selectable_value(selected, i, l).changed() {
                                *error = None;
                            }
                        }
                    });
                if let Some(text) = &preview {
                    ui.add_space(6.0);
                    egui::Frame::NONE
                        .fill(p.canvas)
                        .stroke(egui::Stroke::new(1.0, p.border))
                        .inner_margin(egui::Margin::same(8))
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .max_height(220.0)
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.label(egui::RichText::new(text).monospace());
                                });
                        });
                }
                if let Some(err) = error.as_ref() {
                    ui.colored_label(p.error, err);
                }
                ui.add_space(6.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(&ok_label).clicked() {
                        confirm = true;
                    }
                    if ui.button(&cancel_label).clicked() {
                        cancel = true;
                    }
                });
            });
        if !open || cancel {
            self.encoding_pick = None;
            return;
        }
        if !confirm {
            return;
        }
        let Some(pick) = self.encoding_pick.take() else {
            return;
        };
        match pick {
            EncodingPick::Reopen {
                path,
                head,
                selected,
                suggested,
                ..
            } => {
                let enc = choices.get(selected).copied().unwrap_or(TextEncoding::Utf8);
                match self.ws.open_as(&path, enc) {
                    Ok(_) => self.focus_active(),
                    Err(LoadError::Decode { source, .. }) => {
                        let msg = self.t(
                            "encoding-not-exact",
                            &[("reason", source.to_string().into())],
                        );
                        self.encoding_pick = Some(EncodingPick::Reopen {
                            path,
                            head,
                            selected,
                            error: Some(msg),
                            suggested,
                        });
                    }
                    Err(e) => {
                        self.dialog =
                            Some(Dialog::Error(describe_load_error(&self.l10n, &path, &e)))
                    }
                }
            }
            EncodingPick::Save { selected, .. } => {
                let enc = choices.get(selected).copied().unwrap_or(TextEncoding::Utf8);
                match self.editor_mut().save_with_encoding(enc) {
                    SaveOutcome::Failed(text3_io::SaveError::Unencodable { source, .. }) => {
                        let msg = self.t(
                            "encoding-unencodable",
                            &[
                                ("character", source.character.to_string().into()),
                                ("encoding", source.encoding.into()),
                            ],
                        );
                        // Show where the problem is.
                        self.editor_mut().select(source.index..source.index + 1);
                        self.encoding_pick = Some(EncodingPick::Save {
                            selected,
                            error: Some(msg),
                        });
                    }
                    outcome => {
                        self.handle_save(outcome);
                    }
                }
            }
        }
    }

    /// Ask GitHub for the latest release in the background. `manual`
    /// checks report every outcome; automatic ones only a new version.
    fn start_update_check(&mut self, ctx: &egui::Context, manual: bool) {
        if self.update.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("text3-update".into())
            .spawn(move || {
                let _ = tx.send(text3_app::update::check(env!("CARGO_PKG_VERSION")));
                ctx.request_repaint();
            });
        if spawned.is_ok() {
            self.update = Some((rx, manual));
            if manual {
                self.status = Some((self.t("update-checking", &[]), false));
            }
        }
    }

    fn poll_update(&mut self) {
        let Some((rx, manual)) = &self.update else {
            return;
        };
        let Ok(result) = rx.try_recv() else { return };
        let manual = *manual;
        self.update = None;
        match result {
            Ok(UpdateStatus::Available(r)) => {
                let version = format!(
                    "{}.{}.{}",
                    r.version.major, r.version.minor, r.version.patch
                );
                self.dialog = Some(Dialog::Update(version, r.page));
            }
            Ok(UpdateStatus::UpToDate) if manual => {
                let msg = self.t(
                    "update-up-to-date",
                    &[("version", env!("CARGO_PKG_VERSION").into())],
                );
                self.status = Some((msg, false));
            }
            Ok(UpdateStatus::NoReleases) if manual => {
                self.status = Some((self.t("update-no-releases", &[]), false));
            }
            Err(e) if manual => {
                let reason = match e {
                    UpdateError::Network(m) => m,
                    UpdateError::Invalid => self.t("update-invalid", &[]),
                };
                self.status = Some((self.t("update-failed", &[("reason", reason.into())]), true));
            }
            _ => {}
        }
    }

    /// Run the daily automatic check if the user enabled it.
    pub fn auto_update_check(&mut self, ctx: &egui::Context) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        if !text3_app::STORE
            && self.settings.check_updates
            && now.saturating_sub(self.settings.last_update_check) >= 24 * 3600
        {
            self.settings.last_update_check = now;
            self.save_settings();
            self.start_update_check(ctx, false);
        }
    }

    /// Copy the just-saved file into local history (background thread).
    fn record_history(&self) {
        let (Some(store), true) = (self.history.clone(), self.settings.local_history) else {
            return;
        };
        let Some(path) = self.editor().file().map(|f| f.path.clone()) else {
            return;
        };
        let _ = std::thread::Builder::new()
            .name("text3-history".into())
            .spawn(move || {
                if let Ok(bytes) = std::fs::read(&path)
                    && let Err(e) =
                        store.record(&path, &bytes, text3_io::history::Retention::default())
                {
                    eprintln!("text3: local history not recorded: {e}");
                }
            });
    }

    fn history_window(&mut self, ctx: &egui::Context) {
        if !self.history_open {
            return;
        }
        let Some(path) = self.editor().file().map(|f| f.path.clone()) else {
            self.history_open = false;
            return;
        };
        let versions = self
            .history
            .as_ref()
            .and_then(|h| h.list(&path).ok())
            .unwrap_or_default();
        let now = std::time::SystemTime::now();
        let ago = |t: std::time::SystemTime| -> String {
            let s = now.duration_since(t).map_or(0, |d| d.as_secs()) as i64;
            let (id, n) = match s {
                s if s < 60 => ("history-just-now", 0),
                s if s < 3600 => ("history-minutes-ago", s / 60),
                s if s < 86_400 => ("history-hours-ago", s / 3600),
                s => ("history-days-ago", s / 86_400),
            };
            self.t(id, &[("count", n.into())])
        };
        let rows: Vec<(String, u64, usize)> = versions
            .iter()
            .enumerate()
            .map(|(i, v)| (ago(v.saved), v.size, i))
            .collect();
        let title = self.t("history-title", &[("name", self.display_name().into())]);
        let (open_l, restore_l, empty_l) = (
            self.t("history-open", &[]),
            self.t("history-restore", &[]),
            self.t("history-empty", &[]),
        );
        let mut open = true;
        let mut action: Option<(usize, bool)> = None;
        egui::Window::new(title)
            .id(egui::Id::unique("text3-history"))
            .open(&mut open)
            .collapsible(false)
            .default_width(420.0)
            .show(ctx, |ui| {
                if rows.is_empty() {
                    ui.weak(empty_l);
                }
                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .show(ui, |ui| {
                        egui::Grid::new("history-grid")
                            .num_columns(3)
                            .striped(true)
                            .show(ui, |ui| {
                                for (when, size, i) in &rows {
                                    ui.label(when);
                                    ui.weak(format!("{size} B"));
                                    ui.horizontal(|ui| {
                                        if ui.small_button(&open_l).clicked() {
                                            action = Some((*i, false));
                                        }
                                        if ui.small_button(&restore_l).clicked() {
                                            action = Some((*i, true));
                                        }
                                    });
                                    ui.end_row();
                                }
                            });
                    });
            });
        if !open {
            self.history_open = false;
        }
        let Some((i, restore)) = action else { return };
        let Some(bytes) = versions.get(i).and_then(|v| v.read().ok()) else {
            return;
        };
        let enc = self.editor().encoding();
        let text =
            text3_io::decode_as(&bytes, enc).or_else(|_| text3_io::decode(&bytes).map(|(t, _)| t));
        let Ok(text) = text else {
            self.status = Some((self.t("history-unreadable", &[]), true));
            return;
        };
        if restore {
            // One undoable edit; the user decides whether to save.
            let len = self.editor().document().len_chars();
            self.editor_mut().replace_range(0..len, &text);
            self.editor_mut().select(0..0);
        } else {
            self.ws.new_tab();
            self.editor_mut().execute(Command::Insert(text));
            self.editor_mut().select(0..0);
        }
        self.history_open = false;
        self.focus_active();
    }

    /// Go to Line and Go to Heading, shown in the palette popup.
    fn goto_palette(&mut self, ctx: &egui::Context) {
        use crate::palette::Mode;
        let target = match self.palette.mode {
            Mode::GoToLine => {
                let lines = match &self.log {
                    Some(log) => log.line_count() as i64,
                    None => self.editor().document().rope().len_lines() as i64,
                };
                let hint = self.t("goto-line-hint", &[]);
                let note = self.t("goto-line-note", &[("count", lines.into())]);
                match self.palette.show_input(ctx, &hint, &note) {
                    Some(text) => match text3_app::editor::parse_line_target(&text) {
                        Some(t) => Some(t),
                        None => {
                            self.status = Some((self.t("goto-line-invalid", &[]), true));
                            None
                        }
                    },
                    None => None,
                }
            }
            Mode::Headings => {
                let outline = self.outline();
                let entries: Vec<Entry<usize>> = outline
                    .into_iter()
                    .map(|(level, title, line)| Entry {
                        action: line.saturating_sub(1),
                        label: format!(
                            "{}{title}",
                            "    ".repeat(usize::from(level.saturating_sub(1)))
                        ),
                        shortcut: format!("{line}"),
                        enabled: true,
                    })
                    .collect();
                let hint = self.t(
                    if entries.is_empty() {
                        "goto-heading-none"
                    } else {
                        "goto-heading-hint"
                    },
                    &[],
                );
                self.palette
                    .show(ctx, &entries, &hint)
                    .map(|line| (line, 0))
            }
            Mode::Recent => {
                let paths = self.recent.existing();
                let entries: Vec<Entry<usize>> = paths
                    .iter()
                    .enumerate()
                    .map(|(i, p)| Entry {
                        action: i,
                        label: p
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        shortcut: p.parent().map(short_dir).unwrap_or_default(),
                        enabled: true,
                    })
                    .collect();
                let hint = self.t(
                    if entries.is_empty() {
                        "recent-none"
                    } else {
                        "recent-hint"
                    },
                    &[],
                );
                if let Some(i) = self.palette.show(ctx, &entries, &hint)
                    && let Some(p) = paths.get(i)
                {
                    let p = p.clone();
                    self.open(&p);
                }
                None
            }
            Mode::MacroTimes => {
                let hint = self.t("macro-times-hint", &[]);
                let note = self.t(
                    "macro-times-note",
                    &[("max", (MACRO_TIMES_MAX as i64).into())],
                );
                if let Some(text) = self.palette.show_input(ctx, &hint, &note) {
                    match text.trim().parse::<usize>() {
                        Ok(n) if (1..=MACRO_TIMES_MAX).contains(&n) => {
                            let cmds = self.macro_cmds.clone();
                            self.editor_mut().play_times(&cmds, n);
                            let id = self.ws.active().id;
                            self.active_view(id).reveal();
                        }
                        _ => {
                            self.status = Some((
                                self.t(
                                    "macro-times-invalid",
                                    &[("max", (MACRO_TIMES_MAX as i64).into())],
                                ),
                                true,
                            ))
                        }
                    }
                }
                None
            }
            Mode::MacroSave => {
                let hint = self.t("macro-save-hint", &[]);
                let note = self.t("macro-save-note", &[]);
                if let Some(name) = self.palette.show_input(ctx, &hint, &note) {
                    self.save_macro(&name);
                }
                None
            }
            Mode::MacroPlaySaved | Mode::MacroDeleteSaved => {
                let names: Vec<String> = self
                    .saved_macros
                    .sorted()
                    .into_iter()
                    .map(|m| m.name.clone())
                    .collect();
                let entries: Vec<Entry<usize>> = names
                    .iter()
                    .enumerate()
                    .map(|(i, name)| Entry {
                        action: i,
                        label: name.clone(),
                        shortcut: String::new(),
                        enabled: true,
                    })
                    .collect();
                let hint = self.t(
                    if entries.is_empty() {
                        "macro-saved-none"
                    } else {
                        "macro-saved-hint"
                    },
                    &[],
                );
                let delete = self.palette.mode == Mode::MacroDeleteSaved;
                if let Some(i) = self.palette.show(ctx, &entries, &hint)
                    && let Some(name) = names.get(i)
                {
                    if delete {
                        self.delete_saved_macro(name);
                    } else {
                        self.play_saved_macro(name);
                    }
                }
                None
            }
            Mode::Anything => {
                self.goto_anything(ctx);
                None
            }
            Mode::Commands => None,
        };
        if let (Some((line, _)), Some(log)) = (target, &mut self.log)
            && self.palette.mode == Mode::GoToLine
        {
            log.jump = Some(line);
        } else if let Some((line, col)) = target {
            self.editor_mut().go_to(line, col);
            let id = self.ws.active().id;
            self.active_view(id).reveal();
        }
        if !self.palette.open {
            self.focus_active();
        }
    }

    /// Save the recorded macro as `name`, replacing one with that name.
    /// The file is re-read first so macros saved by another window are
    /// kept.
    fn save_macro(&mut self, name: &str) {
        let mut store = match &self.settings_dir {
            Some(dir) => MacroStore::load(dir),
            None => self.saved_macros.clone(),
        };
        let msg = match store.insert(name, &self.macro_cmds) {
            Ok(name) => match self.write_macros(store) {
                Ok(()) => Ok(self.t("macro-saved", &[("name", name.into())])),
                Err(e) => Err(e),
            },
            Err(MacroError::Empty) => Err(self.t("macro-save-empty", &[])),
            Err(MacroError::InvalidName) => Err(self.t(
                "macro-name-invalid",
                &[("max", (text3_app::macros::NAME_MAX as i64).into())],
            )),
            Err(MacroError::TooLarge) => Err(self.t("macro-save-too-large", &[])),
            Err(MacroError::Full) => Err(self.t(
                "macro-save-full",
                &[("max", (text3_app::macros::MAX_MACROS as i64).into())],
            )),
        };
        self.status = Some(match msg {
            Ok(m) => (m, false),
            Err(m) => (m, true),
        });
    }

    /// Store `store` as the saved macros and write it to the config folder;
    /// the localized error message on failure.
    fn write_macros(&mut self, store: MacroStore) -> Result<(), String> {
        if let Some(dir) = &self.settings_dir
            && let Err(e) = store.save(dir)
        {
            return Err(self.t("macro-save-failed", &[("error", e.to_string().into())]));
        }
        self.saved_macros = store;
        Ok(())
    }

    /// Play a saved macro at the cursor as one undo step. It also becomes
    /// the macro Play Macro repeats.
    fn play_saved_macro(&mut self, name: &str) {
        let Some(m) = self.saved_macros.get(name) else {
            return;
        };
        let cmds = m.steps.clone();
        self.editor_mut().play(&cmds);
        self.macro_cmds = cmds;
        let id = self.ws.active().id;
        self.active_view(id).reveal();
    }

    fn delete_saved_macro(&mut self, name: &str) {
        let mut store = match &self.settings_dir {
            Some(dir) => MacroStore::load(dir),
            None => self.saved_macros.clone(),
        };
        store.remove(name);
        self.status = Some(match self.write_macros(store) {
            Ok(()) => (self.t("macro-deleted", &[("name", name.into())]), false),
            Err(m) => (m, true),
        });
    }

    /// Start a background job whose result arrives as a [`FontMsg`].
    fn font_job(&mut self, job: impl FnOnce() -> FontMsg + Send + 'static) {
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = self.ctx.clone();
        let _ = std::thread::Builder::new()
            .name("text3-fonts".into())
            .spawn(move || {
                let _ = tx.send(job());
                ctx.request_repaint();
            });
        self.font_rx = Some(rx);
    }

    /// At startup: load the saved editor font in the background.
    pub fn start_editor_font(&mut self, ctx: &egui::Context) {
        self.ctx = ctx.clone();
        if self.settings.editor_font.is_some() {
            self.request_editor_font();
        }
    }

    /// Load (or reset) the editor font from the settings.
    fn request_editor_font(&mut self) {
        self.font_wanted = self.settings.editor_font.clone();
        match self.settings.editor_font.clone() {
            Some(family) => self.font_job(move || {
                let face = text3_app::fonts::load_family(&family);
                FontMsg::Loaded(family, face)
            }),
            None => apply_editor_font(&self.ctx, None),
        }
    }

    fn poll_fonts(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.font_rx else { return };
        let Ok(msg) = rx.try_recv() else { return };
        self.font_rx = None;
        match msg {
            FontMsg::Families(f) => self.font_families = Some(f),
            FontMsg::Loaded(family, face) => {
                if self.font_wanted.as_deref() != Some(family.as_str()) {
                    return;
                }
                if face.is_none() {
                    let msg = self.t("settings-font-missing", &[("font", family.into())]);
                    self.status = Some((msg, true));
                }
                apply_editor_font(ctx, face);
            }
        }
    }

    /// Markdown headings (level, title, 1-based line) of the active tab,
    /// parsed once per revision.
    fn outline(&mut self) -> Vec<(u8, String, usize)> {
        let key = (self.ws.active().id, self.editor().document().revision());
        if self.headings.as_ref().map(|(k, _)| *k) != Some(key) {
            let text = self.editor().document().text();
            let outline = text3_markdown::outline(&text3_markdown::parse(&text));
            self.headings = Some((key, outline));
        }
        self.headings
            .as_ref()
            .map(|(_, o)| o.clone())
            .unwrap_or_default()
    }

    /// The Markdown outline side panel (View › Outline).
    fn outline_panel(&mut self, ui: &mut egui::Ui) {
        if !self.outline_open || !Self::is_markdown(self.editor()) || self.log.is_some() {
            return;
        }
        let outline = self.outline();
        let rope = self.editor().document().rope();
        let caret = self
            .editor()
            .selections()
            .primary()
            .head
            .min(rope.len_chars());
        let caret_line = rope.char_to_line(caret) + 1;
        // The section the caret is in: the last heading at or above it.
        let current = outline.iter().rposition(|(_, _, line)| *line <= caret_line);
        let title = self.t("view-outline", &[]);
        let empty = self.t("outline-empty", &[]);
        let close_label = self.t("outline-close", &[]);
        let p = theme::palette(ui.ctx());
        let mut jump = None;
        let mut close = false;
        let focus = &mut self.focus;
        let r = egui::Panel::left("outline")
            .resizable(true)
            .default_size(220.0)
            .frame(
                egui::Frame::NONE
                    .fill(p.surface)
                    .inner_margin(egui::Margin::same(8)),
            )
            .show(ui, |ui| {
                focus.edge(ui, Region::Outline, Edge::Head);
                ui.horizontal(|ui| {
                    ui.strong(title.trim_end_matches('…'));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button("×")
                            .on_hover_text(close_label.as_str())
                            .clicked()
                        {
                            close = true;
                        }
                    });
                });
                ui.separator();
                if outline.is_empty() {
                    ui.colored_label(p.muted, empty);
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (i, (level, text, line)) in outline.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.add_space(f32::from(level.saturating_sub(1)) * 12.0);
                            let r = ui.selectable_label(Some(i) == current, text);
                            if Some(i) == current && self.outline_follow {
                                r.scroll_to_me(None);
                            }
                            if r.clicked() {
                                jump = Some(*line);
                            }
                        });
                    }
                });
                focus.edge(ui, Region::Outline, Edge::Tail);
            });
        self.focus.now.layout.outline = true;
        self.focus
            .now
            .rects
            .push((Region::Outline, r.response.rect));
        self.outline_follow = false;
        if close {
            self.outline_open = false;
            self.focus_active();
        }
        if let Some(line) = jump {
            self.editor_mut().go_to(line.saturating_sub(1), 0);
            let id = self.ws.active().id;
            self.active_view(id).reveal();
            self.focus_active();
        }
    }

    /// Replace the folder search matches in the files of its results: in
    /// the tab for open files (undoable, not saved), on disk otherwise.
    fn replace_in_files(&mut self, ctx: &egui::Context) {
        // The matcher of the shown results, not the query as edited since.
        let Some(matcher) = self.folder.hits_matcher() else {
            return;
        };
        let with = self.folder.replace.clone();
        let history = self.history.clone().filter(|_| self.settings.local_history);
        let same = |a: &Path, b: &Path| {
            a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
        };
        let (mut count, mut files, mut in_tabs) = (0usize, 0usize, 0usize);
        let mut failed: Vec<String> = Vec::new();
        for path in self.folder.hit_files() {
            let tab = self
                .ws
                .tabs()
                .iter()
                .position(|t| t.editor.file().is_some_and(|f| same(&f.path, &path)));
            let n = if let Some(i) = tab {
                let n = self
                    .ws
                    .tab_mut(i)
                    .map_or(0, |t| t.editor.replace_all(&matcher, &with));
                if n > 0 {
                    in_tabs += 1;
                }
                n
            } else {
                match text3_app::replacefiles::replace_in_file(
                    &path,
                    &matcher,
                    &with,
                    history.as_ref(),
                ) {
                    Ok(n) => n,
                    Err(e) => {
                        failed.push(format!("{}: {e}", path.display()));
                        0
                    }
                }
            };
            if n > 0 {
                count += n;
                files += 1;
            }
        }
        let mut msg = self.t(
            "replace-files-done",
            &[
                ("count", (count as i64).into()),
                ("files", (files as i64).into()),
                ("open", (in_tabs as i64).into()),
            ],
        );
        if let Some(first) = failed.first() {
            msg.push(' ');
            msg.push_str(&self.t(
                "replace-files-failed",
                &[
                    ("failed", (failed.len() as i64).into()),
                    ("first", first.clone().into()),
                ],
            ));
        }
        self.status = Some((msg, !failed.is_empty()));
        // Show what is left.
        self.folder.start(ctx);
    }

    /// Reload the active file from disk in its encoding (no undo across).
    fn revert(&mut self) {
        let Some(path) = self.editor().file().map(|f| f.path.clone()) else {
            return;
        };
        let enc = self.editor().encoding();
        match self.ws.open_as(&path, enc) {
            Ok(_) => {
                let msg = self.t("revert-done", &[("name", self.display_name().into())]);
                self.status = Some((msg, false));
                self.focus_active();
            }
            Err(e) => self.dialog = Some(Dialog::Error(describe_load_error(&self.l10n, &path, &e))),
        }
    }

    /// Save every modified tab that has a file, stopping at the first one
    /// that needs the user (conflict, error).
    fn save_all(&mut self) {
        let current = self.ws.active_index();
        let dirty: Vec<TabId> = self
            .ws
            .tabs()
            .iter()
            .filter(|t| t.editor.document().is_dirty() && t.editor.file().is_some())
            .map(|t| t.id)
            .collect();
        let mut saved = 0i64;
        for id in dirty {
            let Some(i) = self.ws.position(id) else {
                continue;
            };
            self.ws.activate(i);
            if !self.save() {
                return; // its dialog or error is now showing for this tab
            }
            saved += 1;
        }
        self.ws.activate(current);
        self.status = Some((
            self.t("status-saved-all", &[("count", saved.into())]),
            false,
        ));
    }

    fn about_window(&mut self, ctx: &egui::Context) {
        if !self.about_open {
            return;
        }
        const REPO: &str = "https://github.com/Zanttux/text3";
        let p = theme::palette(ctx);
        let version = self.t(
            "about-version",
            &[("version", env!("CARGO_PKG_VERSION").into())],
        );
        let logo = self
            .about_logo
            .get_or_insert_with(|| {
                let png = include_bytes!("../../../assets/brand/about-lockup.png");
                let color = image::load_from_memory(png).map_or_else(
                    |_| egui::ColorImage::new([1, 1], vec![egui::Color32::TRANSPARENT]),
                    |i| {
                        let rgba = i.into_rgba8();
                        let size = [rgba.width() as usize, rgba.height() as usize];
                        egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw())
                    },
                );
                ctx.load_texture("text3-about-logo", color, egui::TextureOptions::LINEAR)
            })
            .clone();
        let (license, ai, notices, repo_l) = (
            self.t("about-license", &[]),
            self.t("about-ai", &[]),
            self.t("about-notices", &[]),
            self.t("about-source", &[]),
        );
        let mut open = true;
        let mut link = false;
        egui::Window::new(self.t("app-about", &[]).trim_end_matches('…').to_owned())
            .id(egui::Id::unique("text3-about"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(380.0)
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(6.0);
                    // The lockup sits on a white plate, also in dark themes
                    // (docs/BRAND.md: never recolour the artwork).
                    egui::Frame::new()
                        .fill(egui::Color32::WHITE)
                        .corner_radius(8.0)
                        .inner_margin(12.0)
                        .show(ui, |ui| {
                            let [w, h] = logo.size();
                            let width = 280.0;
                            let size = egui::vec2(width, width * h as f32 / w.max(1) as f32);
                            let resp = ui.add(egui::Image::new((logo.id(), size)));
                            resp.widget_info(|| {
                                egui::WidgetInfo::labeled(egui::Role::Image, true, "Text³")
                            });
                        });
                    ui.add_space(8.0);
                    ui.label(version);
                    ui.add_space(10.0);
                    ui.label(license);
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(ai).color(p.muted));
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(notices).color(p.muted));
                    ui.add_space(8.0);
                    if ui.link(repo_l).clicked() {
                        link = true;
                    }
                    ui.add_space(6.0);
                });
            });
        if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.about_open = false;
        }
        if link {
            // The usual confirmation before leaving the app.
            self.dialog = Some(Dialog::OpenLink(REPO.to_owned()));
        }
    }

    fn replace_preview_window(&mut self, ctx: &egui::Context) {
        let Some(pv) = &self.replace_preview else {
            return;
        };
        let p = theme::palette(ctx);
        let n = pv.count as i64;
        let title = self.t("replace-preview-title", &[("count", n.into())]);
        let more = pv.lines.len() == PREVIEW_LINES;
        let (confirm_l, cancel_l, more_l) = (
            self.t("replace-preview-confirm", &[("count", n.into())]),
            self.t("button-cancel", &[]),
            self.t("replace-preview-more", &[]),
        );
        let font = egui::FontId::monospace(self.settings.font_size * 0.9);
        let mut decision: Option<bool> = None;
        let mut open = true;
        egui::Window::new(title)
            .id(egui::Id::unique("text3-replace-preview"))
            .open(&mut open)
            .collapsible(false)
            .default_width(640.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(360.0)
                    .show(ui, |ui| {
                        egui::Grid::new("replace-preview-grid")
                            .num_columns(2)
                            .spacing([12.0, 2.0])
                            .show(ui, |ui| {
                                let cut = |s: &str| -> String {
                                    let t: String = s.chars().take(160).collect();
                                    if s.chars().count() > 160 {
                                        format!("{t}…")
                                    } else {
                                        t
                                    }
                                };
                                for (line, before, after) in &pv.lines {
                                    ui.label(
                                        egui::RichText::new(format!("{}", line + 1))
                                            .font(font.clone())
                                            .color(p.muted),
                                    );
                                    ui.vertical(|ui| {
                                        ui.label(
                                            egui::RichText::new(format!("− {}", cut(before)))
                                                .font(font.clone())
                                                .color(p.error),
                                        );
                                        ui.label(
                                            egui::RichText::new(format!("+ {}", cut(after)))
                                                .font(font.clone())
                                                .color(p.accent),
                                        );
                                    });
                                    ui.end_row();
                                }
                            });
                        if more {
                            ui.weak(more_l);
                        }
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button(confirm_l).clicked() {
                        decision = Some(true);
                    }
                    if ui.button(cancel_l).clicked() {
                        decision = Some(false);
                    }
                });
            });
        // Esc cancels; confirming takes the button (Enter may belong to
        // the find field underneath).
        if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            decision = Some(false);
        }
        let Some(go) = decision else { return };
        let Some(pv) = self.replace_preview.take() else {
            return;
        };
        if !go {
            return;
        }
        let fresh =
            self.ws.active().id == pv.tab && self.editor().document().revision() == pv.revision;
        if !fresh {
            self.status = Some((self.t("replace-preview-stale", &[]), true));
            return;
        }
        self.editor_mut().apply_replacement(&pv.tx);
        let msg = self.t("status-replaced", &[("count", (pv.count as i64).into())]);
        self.status = Some((msg, false));
        let id = self.ws.active().id;
        self.active_view(id).reveal();
    }

    fn data_kind(ed: &Editor) -> Option<DataKind> {
        let ext = ed.file()?.path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "csv" => Some(DataKind::Csv),
            "tsv" | "tab" => Some(DataKind::Tsv),
            "json" | "geojson" => Some(DataKind::Json),
            "xml" | "xsd" | "xsl" | "xslt" | "svg" | "plist" | "xaml" | "csproj" | "fsproj"
            | "vbproj" | "props" | "targets" | "resx" | "xhtml" | "rss" | "atom" | "gpx"
            | "kml" => Some(DataKind::Xml),
            "yaml" | "yml" => Some(DataKind::Yaml),
            _ => None,
        }
    }

    /// Draw the structured view if enabled for the active tab.
    /// The log viewer, when open, replaces the editor area.
    fn log_area(&mut self, ui: &mut egui::Ui) -> bool {
        let Some(log) = &mut self.log else {
            return false;
        };
        log.tick();
        let p = theme::palette(ui.ctx());
        let name = log
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let lines = log.line_count() as i64;
        let tr_ = |id: &str, args: &[(&str, FluentValue<'static>)]| tr(&self.l10n, id, args);
        let count = tr_("log-lines", &[("count", lines.into())]);
        let enc_label = if log.encoding == text3_io::logfile::UTF_8 {
            "UTF-8".to_owned()
        } else if log.encoding == text3_io::logfile::UTF_16LE {
            "UTF-16 LE".to_owned()
        } else if log.encoding == text3_io::logfile::UTF_16BE {
            "UTF-16 BE".to_owned()
        } else {
            let id = format!("enc-{}", log.encoding.name().to_ascii_lowercase());
            format!("{} ({})", tr_(&id, &[]), log.encoding.name())
        };
        let progress = log
            .progress()
            .map(|pc| tr_("log-indexing", &[("percent", i64::from(pc).into())]));
        let matches = log.matches().map(|(n, running)| {
            let t = tr_("log-matches", &[("count", (n as i64).into())]);
            if running { format!("{t} …") } else { t }
        });
        let (hint, regex_l, follow_l, close_l, bad_l) = (
            tr_("log-filter-hint", &[]),
            tr_("find-regex", &[]),
            tr_("log-follow", &[]),
            tr_("log-close", &[]),
            tr_("log-bad-pattern", &[]),
        );
        let mut close = false;
        let mut changed = false;
        egui::Frame::NONE
            .fill(p.surface)
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(&name);
                    ui.colored_label(p.muted, &count);
                    ui.colored_label(p.muted, &enc_label);
                    if let Some(pr) = &progress {
                        ui.colored_label(p.muted, pr);
                    }
                    ui.separator();
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut log.query)
                            .hint_text(hint)
                            .desired_width(220.0),
                    );
                    changed |= r.changed();
                    changed |= ui.checkbox(&mut log.regex, regex_l).changed();
                    if log.bad_pattern {
                        ui.colored_label(p.error, bad_l);
                    } else if let Some(m) = &matches {
                        ui.colored_label(p.muted, m);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(close_l).clicked() {
                            close = true;
                        }
                        ui.checkbox(&mut log.follow, follow_l);
                    });
                });
            });
        if changed {
            log.set_query();
        }
        if let Some(e) = log.error.take() {
            self.status = Some((e, true));
        }
        let font = egui::FontId::monospace(self.settings.font_size);
        let row_h = ui.fonts_mut(|f| f.row_height(&font));
        let rows = log.rows();
        let digits = log.line_count().max(1).to_string().len();
        let follow = log.follow;
        // Rows are exactly `row_h` apart, so offsets are simple products.
        ui.spacing_mut().item_spacing.y = 0.0;
        let mut area = egui::ScrollArea::both().auto_shrink(false);
        if let Some(line) = log.jump.take() {
            log.follow = false;
            area = area.vertical_scroll_offset(log.row_of_line(line) as f32 * row_h);
        } else if follow {
            // Like `tail -f`: stay at the end; turn Follow off to scroll.
            let end = rows as f32 * row_h - ui.available_height() + row_h;
            area = area.vertical_scroll_offset(end.max(0.0));
        }
        area.show_rows(ui, row_h, rows, |ui, range| {
            // Every row exactly `row_h` tall (egui's minimum is larger).
            ui.spacing_mut().interact_size.y = row_h;
            for (line, text, cut) in log.row_lines(range) {
                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(format!("{:>digits$}", line + 1))
                            .font(font.clone())
                            .color(p.muted),
                    );
                    ui.add_space(10.0);
                    let color = match text3_io::logfile::level(&text) {
                        Some(text3_io::logfile::Level::Error) => p.error,
                        Some(text3_io::logfile::Level::Warn) => p.warning,
                        Some(text3_io::logfile::Level::Debug) => p.muted,
                        _ => p.text,
                    };
                    let shown = if cut { format!("{text}…") } else { text };
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(shown).font(font.clone()).color(color),
                        )
                        .wrap_mode(egui::TextWrapMode::Extend)
                        .selectable(true),
                    );
                });
            }
        });
        if close {
            self.log = None;
            self.focus_active();
        }
        true
    }

    /// The tree or grid view of a data tab.
    fn structured_area(&mut self, ui: &mut egui::Ui, id: TabId) {
        let Some(i) = self.ws.position(id) else {
            return;
        };
        let ed = &self.ws.tabs()[i].editor;
        let Some(kind) = Self::data_kind(ed) else {
            return;
        };
        let rev = ed.document().revision();
        let override_delim = self.csv_delimiter.get(&id).copied();
        let fresh = self
            .structured_cache
            .get(&id)
            .is_some_and(|(r, d, _)| *r == rev && *d == override_delim);
        if !fresh {
            let ed = &self.ws.tabs()[i].editor;
            let text = ed.document().text();
            let read_only = ed.file().is_some_and(|f| {
                std::fs::metadata(&f.path).is_ok_and(|m| m.permissions().readonly())
            });
            let data = match kind {
                DataKind::Json => Structured::Json(text3_tools::json::doc(&text), read_only),
                DataKind::Xml => Structured::Xml(text3_tools::xml::doc(&text), read_only),
                DataKind::Yaml => Structured::Yaml(text3_tools::yaml::doc(&text), read_only),
                DataKind::Csv | DataKind::Tsv => {
                    let hint = override_delim.or((kind == DataKind::Tsv).then_some('\t'));
                    let d = text3_tools::csv::detect_delimiter(&text, hint);
                    Structured::Csv(text3_tools::csv::parse(&text, d), read_only)
                }
            };
            self.structured_cache
                .insert(id, (rev, override_delim, data));
        }
        let p = theme::palette(ui.ctx());
        let size = self.settings.font_size;
        let l10n = &self.l10n;
        let Some((_, _, data)) = self.structured_cache.get(&id) else {
            return;
        };
        let mut pick = None;
        let mut grid_actions = Vec::new();
        let mut tree_actions = Vec::new();
        let mut xml_actions = Vec::new();
        let mut yaml_actions = Vec::new();
        let ax_tr = |id: &str, args: &[(&str, FluentValue<'_>)]| tr(l10n, id, args);
        match data {
            Structured::Csv(table, read_only) => {
                egui::Frame::NONE
                    .fill(p.surface)
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(tr(l10n, "csv-delimiter", &[]));
                            for (c, name) in [(',', ","), (';', ";"), ('\t', "Tab"), ('|', "|")] {
                                if ui.selectable_label(table.delimiter == c, name).clicked() {
                                    pick = Some(c);
                                }
                            }
                            // The first row is shown as the header.
                            let rows = table.rows.len().saturating_sub(1) as i64;
                            ui.colored_label(
                                p.muted,
                                tr(l10n, "csv-rows", &[("count", rows.into())]),
                            );
                            if let Some(line) = table.unterminated_quote {
                                let msg =
                                    tr(l10n, "csv-unterminated", &[("line", (line as i64).into())]);
                                ui.colored_label(p.error, msg);
                            }
                            if table.truncated {
                                ui.colored_label(p.muted, tr(l10n, "csv-truncated", &[]));
                            }
                            if *read_only {
                                ui.colored_label(p.muted, tr(l10n, "csv-read-only", &[]));
                            }
                        });
                    });
                let labels = crate::structured::GridLabels {
                    edit_cell: tr(l10n, "csv-edit-cell", &[]),
                    insert_row_below: tr(l10n, "csv-insert-row-below", &[]),
                    delete_row: tr(l10n, "csv-delete-row", &[]),
                };
                let editable = !*read_only && table.unterminated_quote.is_none();
                let state = self.csv_grids.entry(id).or_default();
                grid_actions = crate::structured::csv(
                    ui,
                    grid_id(id),
                    table,
                    p,
                    size,
                    true,
                    editable,
                    state,
                    &labels,
                    &ax_tr,
                );
            }
            Structured::Json(Ok(doc), read_only) => {
                let more = |n: usize| tr(l10n, "json-more", &[("count", (n as i64).into())]);
                if *read_only {
                    egui::Frame::NONE
                        .fill(p.surface)
                        .inner_margin(egui::Margin::symmetric(8, 6))
                        .show(ui, |ui| {
                            ui.colored_label(p.muted, tr(l10n, "json-read-only", &[]));
                        });
                }
                let labels = crate::structured::TreeLabels {
                    edit_value: tr(l10n, "json-edit-value", &[]),
                    rename_key: tr(l10n, "json-rename-key", &[]),
                    add_after: tr(l10n, "json-add-after", &[]),
                    add_inside: tr(l10n, "json-add-inside", &[]),
                    delete: tr(l10n, "json-delete-node", &[]),
                    invalid_value: tr(l10n, "json-invalid-value", &[]),
                    duplicate_key: tr(l10n, "json-duplicate-key", &[]),
                };
                let state = self.trees.entry(id).or_default();
                tree_actions = crate::structured::json(
                    ui,
                    tree_id(id),
                    doc,
                    p,
                    !*read_only,
                    state,
                    &labels,
                    &more,
                    &ax_tr,
                );
            }
            Structured::Xml(Ok(doc), read_only) => {
                let more = |n: usize| tr(l10n, "json-more", &[("count", (n as i64).into())]);
                if *read_only {
                    egui::Frame::NONE
                        .fill(p.surface)
                        .inner_margin(egui::Margin::symmetric(8, 6))
                        .show(ui, |ui| {
                            ui.colored_label(p.muted, tr(l10n, "json-read-only", &[]));
                        });
                }
                let labels = crate::structured::XmlLabels {
                    edit_value: tr(l10n, "xml-edit-value", &[]),
                    edit_text: tr(l10n, "xml-edit-text", &[]),
                    rename: tr(l10n, "xml-rename", &[]),
                    add_attribute: tr(l10n, "xml-add-attribute", &[]),
                    add_child: tr(l10n, "xml-add-child", &[]),
                    add_after: tr(l10n, "xml-add-after", &[]),
                    delete: tr(l10n, "xml-delete", &[]),
                    invalid_name: tr(l10n, "xml-invalid-name", &[]),
                    duplicate_attribute: tr(l10n, "xml-duplicate-attribute", &[]),
                    not_well_formed: tr(l10n, "xml-not-well-formed", &[]),
                };
                let state = self.trees.entry(id).or_default();
                xml_actions = crate::structured::xml(
                    ui,
                    tree_id(id),
                    doc,
                    p,
                    !*read_only,
                    state,
                    &labels,
                    &more,
                    &ax_tr,
                );
            }
            Structured::Yaml(Ok(doc), read_only) => {
                let more = |n: usize| tr(l10n, "json-more", &[("count", (n as i64).into())]);
                if *read_only {
                    egui::Frame::NONE
                        .fill(p.surface)
                        .inner_margin(egui::Margin::symmetric(8, 6))
                        .show(ui, |ui| {
                            ui.colored_label(p.muted, tr(l10n, "json-read-only", &[]));
                        });
                }
                let labels = crate::structured::YamlLabels {
                    edit_value: tr(l10n, "json-edit-value", &[]),
                    rename_key: tr(l10n, "json-rename-key", &[]),
                    add_after: tr(l10n, "json-add-after", &[]),
                    add_inside: tr(l10n, "json-add-inside", &[]),
                    delete: tr(l10n, "json-delete-node", &[]),
                    duplicate_key: tr(l10n, "json-duplicate-key", &[]),
                    key_too_long: tr(l10n, "yaml-key-too-long", &[]),
                    not_preserved: tr(l10n, "yaml-not-preserved", &[]),
                    locks: yaml_lock_labels(l10n),
                };
                let state = self.trees.entry(id).or_default();
                yaml_actions = crate::structured::yaml(
                    ui,
                    tree_id(id),
                    doc,
                    p,
                    !*read_only,
                    state,
                    &labels,
                    &more,
                    &ax_tr,
                );
            }
            Structured::Json(Err(e), _) => {
                let msg = tr(
                    l10n,
                    "tools-json-error",
                    &[
                        ("line", (e.line as i64).into()),
                        ("column", (e.column as i64).into()),
                        ("reason", tr(l10n, &json_reason_id(e.message), &[]).into()),
                    ],
                );
                ui.colored_label(p.error, msg);
            }
            Structured::Yaml(Err(e), _) | Structured::Xml(Err(e), _) => {
                let format = match data {
                    Structured::Xml(..) => "format-xml",
                    _ => "format-yaml",
                };
                let msg = tr(
                    l10n,
                    "tools-tree-error",
                    &[
                        ("format", tr(l10n, format, &[]).into()),
                        ("line", (e.line as i64).into()),
                        ("column", (e.column as i64).into()),
                        ("reason", e.message.clone().into()),
                    ],
                );
                egui::Frame::NONE
                    .inner_margin(egui::Margin::same(12))
                    .show(ui, |ui| ui.colored_label(p.error, msg));
            }
        }
        if let Some(c) = pick {
            self.csv_delimiter.insert(id, c);
        }
        for a in grid_actions {
            self.apply_grid_action(id, a);
        }
        for a in tree_actions {
            self.apply_tree_action(id, a);
        }
        for a in xml_actions {
            self.apply_xml_action(id, a);
        }
        for a in yaml_actions {
            self.apply_yaml_action(id, a);
        }
    }

    /// Apply a YAML tree change to the tab's text as one undoable edit,
    /// computed from the tree shown (the cache is for this revision).
    fn apply_yaml_action(&mut self, id: TabId, action: crate::structured::TreeAction) {
        use crate::structured::{Field, TreeAction};
        use text3_tools::yaml::Kind;
        let Some((rev, _, Structured::Yaml(Ok(doc), false))) = self.structured_cache.get(&id)
        else {
            return;
        };
        let Some(i) = self.ws.position(id) else {
            return;
        };
        if self.ws.tabs()[i].editor.document().revision() != *rev {
            return;
        }
        // The edits, the node to select afterwards and a field to open on it.
        let (edits, select, open) = match &action {
            TreeAction::SetValue { path, text } => (
                doc.find(path)
                    .and_then(|n| doc.set_value(n, text).ok().flatten()),
                Some(path.clone()),
                None,
            ),
            TreeAction::RenameKey { path, name } => (
                doc.find(path)
                    .and_then(|n| doc.rename_key(n, name).ok().flatten()),
                Some(path.clone()),
                None,
            ),
            TreeAction::Delete(path) => {
                let n = doc.find(path);
                // Select the next sibling, else the previous one, else the parent.
                let siblings = n
                    .and_then(|n| doc.items[n].parent)
                    .map_or(0, |p| doc.items[p].children.len());
                let mut sel = path.clone();
                match sel.last_mut() {
                    Some(k) if *k + 1 < siblings => {}
                    Some(k) if *k > 0 => *k -= 1,
                    _ => {
                        sel.pop();
                    }
                }
                (n.and_then(|n| doc.delete(n).ok()), Some(sel), None)
            }
            TreeAction::AddAfter(path) => {
                let n = doc.find(path);
                let parent = n.and_then(|n| doc.items[n].parent);
                let map = parent.is_some_and(|p| doc.items[p].kind == Kind::Mapping);
                let key = parent.filter(|_| map).map(|p| doc.unique_key(p, "key"));
                let mut sel = path.clone();
                if let Some(k) = sel.last_mut() {
                    *k += 1;
                }
                let field = if map { Field::Key } else { Field::Value };
                (
                    n.and_then(|n| doc.add_after(n, key.as_deref()).ok()),
                    Some(sel.clone()),
                    Some((sel, field)),
                )
            }
            TreeAction::AddInside(path) => {
                let n = doc.find(path);
                let map = n.is_some_and(|n| doc.items[n].kind == Kind::Mapping);
                let key = n.filter(|_| map).map(|n| doc.unique_key(n, "key"));
                let mut sel = path.clone();
                sel.push(n.map_or(0, |n| doc.items[n].children.len()));
                let field = if map { Field::Key } else { Field::Value };
                (
                    n.and_then(|n| doc.add_child(n, key.as_deref()).ok()),
                    Some(sel.clone()),
                    Some((sel, field)),
                )
            }
            TreeAction::AddAttribute(_) => return,
        };
        let Some(edits) = edits.filter(|e| !e.is_empty()) else {
            return;
        };
        let Some(tab) = self.ws.tab_mut(i) else {
            return;
        };
        if let [e] = edits.as_slice() {
            tab.editor.replace_range(e.range.clone(), &e.text);
        } else {
            let mut tx = text3_core::Transaction::new();
            for e in edits.iter().rev() {
                tx.push(text3_core::Change::replace(e.range.clone(), e.text.clone()));
            }
            tab.editor.apply_replacement(&tx);
        }
        let state = self.trees.entry(id).or_default();
        state.selected = select;
        state.selected_field = open.as_ref().map(|(_, f)| *f);
        state.pending = open;
    }

    /// Apply an XML tree change to the tab's text as one undoable edit
    /// (renaming an element changes both its tags in that one step).
    fn apply_xml_action(&mut self, id: TabId, action: crate::structured::TreeAction) {
        use crate::structured::{Field, TreeAction};
        use text3_tools::xml::Kind;
        let Some((rev, _, Structured::Xml(Ok(doc), false))) = self.structured_cache.get(&id) else {
            return;
        };
        let Some(i) = self.ws.position(id) else {
            return;
        };
        if self.ws.tabs()[i].editor.document().revision() != *rev {
            return;
        }
        // The edits, the node to select afterwards, and for a new node the
        // kind to find at the inserted text (its name field opens).
        let (edits, select, added) = match &action {
            TreeAction::SetValue { path, text } => (
                doc.find(path)
                    .and_then(|n| doc.set_value(n, text).ok().flatten()),
                Some(path.clone()),
                None,
            ),
            TreeAction::RenameKey { path, name } => (
                doc.find(path)
                    .and_then(|n| doc.rename(n, name).ok().flatten()),
                Some(path.clone()),
                None,
            ),
            TreeAction::Delete(path) => {
                let n = doc.find(path);
                // Select the next sibling, else the previous one, else the parent.
                let siblings = n
                    .and_then(|n| doc.items[n].parent)
                    .map_or(0, |p| doc.items[p].children.len());
                let mut sel = path.clone();
                match sel.last_mut() {
                    Some(k) if *k + 1 < siblings => {}
                    Some(k) if *k > 0 => *k -= 1,
                    _ => {
                        sel.pop();
                    }
                }
                (n.and_then(|n| doc.delete(n).ok()), Some(sel), None)
            }
            TreeAction::AddAttribute(path) => {
                // On an attribute: add to its element.
                let e = doc.find(path).map(|n| match doc.items[n].kind {
                    Kind::Attribute => doc.items[n].parent.unwrap_or(n),
                    _ => n,
                });
                let edits = e.and_then(|e| {
                    let name = doc.unique_attribute(e, "attribute");
                    doc.add_attribute(e, &name, "").ok()
                });
                (edits, None, Some(Kind::Attribute))
            }
            TreeAction::AddAfter(path) => (
                doc.find(path)
                    .and_then(|n| doc.add_after(n, "element").ok()),
                None,
                Some(Kind::Element),
            ),
            TreeAction::AddInside(path) => (
                doc.find(path)
                    .and_then(|n| doc.add_child(n, "element").ok()),
                None,
                Some(Kind::Element),
            ),
        };
        let Some(edits) = edits.filter(|e| !e.is_empty()) else {
            return;
        };
        // Where the new node starts: the inserted attribute's leading space,
        // or the new element's `<`.
        let at = edits.first().map(|e| {
            let skip = match added {
                Some(Kind::Element) => e.text.find('<').unwrap_or(0),
                _ => 0,
            };
            e.range.start + e.text[..skip].chars().count()
        });
        let Some(tab) = self.ws.tab_mut(i) else {
            return;
        };
        if let [e] = edits.as_slice() {
            tab.editor.replace_range(e.range.clone(), &e.text);
        } else {
            let mut tx = text3_core::Transaction::new();
            for e in edits.iter().rev() {
                tx.push(text3_core::Change::replace(e.range.clone(), e.text.clone()));
            }
            tab.editor.apply_replacement(&tx);
        }
        let mut select = select;
        let mut open = None;
        if let (Some(kind), Some(at)) = (added, at) {
            let text = tab.editor.document().text();
            if let Ok(new) = text3_tools::xml::doc(&text)
                && let Some(n) = (0..new.items.len())
                    .find(|&n| new.items[n].kind == kind && new.items[n].spans.whole.start == at)
            {
                let path = new.path_of(n);
                select = Some(path.clone());
                open = Some((path, Field::Key));
            }
        }
        let state = self.trees.entry(id).or_default();
        state.selected = select;
        state.selected_field = open.as_ref().map(|(_, f)| *f);
        state.pending = open;
    }

    /// Apply a JSON tree change to the tab's text as one undoable edit,
    /// computed from the tree shown (the cache is for this revision).
    fn apply_tree_action(&mut self, id: TabId, action: crate::structured::TreeAction) {
        use crate::structured::{Field, TreeAction};
        use text3_tools::json::Kind;
        let Some((rev, _, Structured::Json(Ok(doc), false))) = self.structured_cache.get(&id)
        else {
            return;
        };
        let Some(i) = self.ws.position(id) else {
            return;
        };
        if self.ws.tabs()[i].editor.document().revision() != *rev {
            return;
        }
        // The edit, the node to select afterwards and a field to open on it.
        let (edit, select, open) = match &action {
            TreeAction::SetValue { path, text } => (
                doc.find(path)
                    .and_then(|n| doc.set_value(n, text).ok().flatten()),
                Some(path.clone()),
                None,
            ),
            TreeAction::RenameKey { path, name } => (
                doc.find(path)
                    .and_then(|n| doc.rename_key(n, name).ok().flatten()),
                Some(path.clone()),
                None,
            ),
            TreeAction::Delete(path) => {
                let n = doc.find(path);
                // Select the next sibling, else the previous one, else the parent.
                let siblings = n
                    .and_then(|n| doc.items[n].parent)
                    .map_or(0, |p| doc.items[p].children.len());
                let mut sel = path.clone();
                match sel.last_mut() {
                    Some(k) if *k + 1 < siblings => {}
                    Some(k) if *k > 0 => *k -= 1,
                    _ => {
                        sel.pop();
                    }
                }
                (n.and_then(|n| doc.delete(n)), Some(sel), None)
            }
            TreeAction::AddAfter(path) => {
                let n = doc.find(path);
                let parent = n.and_then(|n| doc.items[n].parent);
                let object = parent.is_some_and(|p| doc.items[p].kind == Kind::Object);
                let key = parent.filter(|_| object).map(|p| doc.unique_key(p, "key"));
                let mut sel = path.clone();
                if let Some(k) = sel.last_mut() {
                    *k += 1;
                }
                let field = if object { Field::Key } else { Field::Value };
                (
                    n.and_then(|n| doc.add_after(n, key.as_deref(), "null").ok()),
                    Some(sel.clone()),
                    Some((sel, field)),
                )
            }
            TreeAction::AddInside(path) => {
                let n = doc.find(path);
                let object = n.is_some_and(|n| doc.items[n].kind == Kind::Object);
                let key = n.filter(|_| object).map(|n| doc.unique_key(n, "key"));
                let mut sel = path.clone();
                sel.push(n.map_or(0, |n| doc.items[n].children.len()));
                let field = if object { Field::Key } else { Field::Value };
                (
                    n.and_then(|n| doc.add_child(n, key.as_deref(), "null").ok()),
                    Some(sel.clone()),
                    Some((sel, field)),
                )
            }
            TreeAction::AddAttribute(_) => return,
        };
        let Some(edit) = edit else {
            return;
        };
        if let Some(tab) = self.ws.tab_mut(i) {
            tab.editor.replace_range(edit.range, &edit.text);
        }
        let state = self.trees.entry(id).or_default();
        state.selected = select;
        state.selected_field = open.as_ref().map(|(_, f)| *f);
        state.pending = open;
    }

    /// Apply a grid change to the tab's text as one undoable edit, computed
    /// from the table the grid showed (the cache is for this revision).
    fn apply_grid_action(&mut self, id: TabId, action: crate::structured::GridAction) {
        use crate::structured::GridAction;
        let Some((rev, _, Structured::Csv(table, false))) = self.structured_cache.get(&id) else {
            return;
        };
        let Some(i) = self.ws.position(id) else {
            return;
        };
        if self.ws.tabs()[i].editor.document().revision() != *rev {
            return;
        }
        let edit = match &action {
            GridAction::SetCell { row, col, value } => table.edit_cell(*row, *col, value),
            GridAction::InsertRowAfter(r) => table.insert_row_after(*r),
            GridAction::DeleteRow(r) => table.delete_row(*r),
        };
        let Some(edit) = edit else {
            return;
        };
        if let Some(tab) = self.ws.tab_mut(i) {
            tab.editor.replace_range(edit.range, &edit.text);
        }
        if let (GridAction::InsertRowAfter(r), Some(g)) = (&action, self.csv_grids.get_mut(&id)) {
            g.selected = Some((r + 1, g.selected.map_or(0, |(_, c)| c)));
        }
    }

    /// Show two open files side by side (`text3 --diff LEFT RIGHT`). Does
    /// nothing if either is not open (its load error is already reported).
    pub fn compare_files(&mut self, left: &Path, right: &Path) {
        let same = |a: &Path, b: &Path| {
            a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
        };
        let find = |p: &Path| {
            self.ws
                .tabs()
                .iter()
                .find(|t| t.editor.file().is_some_and(|f| same(&f.path, p)))
                .map(|t| t.id)
        };
        let (Some(l), Some(r)) = (find(left), find(right)) else {
            return;
        };
        if l == r {
            return;
        }
        if let Some(i) = self.ws.position(l) {
            self.ws.activate(i);
        }
        self.start_compare(Side::Tab(r));
    }

    fn start_compare(&mut self, right: Side) {
        self.compare = Some(Compare {
            left: self.ws.active().id,
            right,
            revs: (Revision(u64::MAX), Revision(u64::MAX)),
            diff: Default::default(),
            texts: Default::default(),
            ignore_whitespace: false,
        });
    }

    fn tab_text(&self, id: TabId) -> Option<(Revision, String)> {
        let i = self.ws.position(id)?;
        let d = self.ws.tabs()[i].editor.document();
        Some((d.revision(), d.text()))
    }

    fn compare_picker(&mut self, ctx: &egui::Context) {
        if !self.compare_pick {
            return;
        }
        let active = self.ws.active().id;
        let others: Vec<(TabId, String)> = self
            .ws
            .tabs()
            .iter()
            .filter(|t| t.id != active)
            .map(|t| (t.id, self.name_of(&t.editor)))
            .collect();
        let mut open = true;
        let mut chosen = None;
        egui::Window::new(self.t(
            "compare-pick-title",
            &[("name", self.display_name().into())],
        ))
        .id(egui::Id::unique("text3-compare-pick"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .show(ctx, |ui| {
            for (id, name) in &others {
                if ui.button(name).clicked() {
                    chosen = Some(*id);
                }
            }
        });
        if let Some(id) = chosen {
            self.start_compare(Side::Tab(id));
            self.compare_pick = false;
        } else if !open {
            self.compare_pick = false;
        }
    }

    /// Draw the comparison if the active tab is its left side. Returns
    /// whether it was drawn.
    fn compare_area(&mut self, ui: &mut egui::Ui) -> bool {
        let Some(c) = &self.compare else { return false };
        if c.left != self.ws.active().id {
            return false;
        }
        let left = c.left;
        let rev_of = |id: TabId| {
            self.ws
                .position(id)
                .map(|i| self.ws.tabs()[i].editor.document().revision())
        };
        let right_rev = match &c.right {
            Side::Tab(id) => rev_of(*id),
            Side::Text { .. } => Some(Revision(0)),
        };
        let (Some(lrev), Some(rrev)) = (rev_of(left), right_rev) else {
            self.compare = None;
            return false;
        };
        // Copy the texts only when a side changed.
        if c.revs != (lrev, rrev) {
            let ltext = self.tab_text(left).map(|t| t.1).unwrap_or_default();
            let rtext = match &c.right {
                Side::Tab(id) => self.tab_text(*id).map(|t| t.1).unwrap_or_default(),
                Side::Text { text, .. } => text.clone(),
            };
            if let Some(c) = &mut self.compare {
                c.diff = text3_tools::diff::diff_lines_with(&ltext, &rtext, c.ignore_whitespace);
                c.revs = (lrev, rrev);
                c.texts = (std::sync::Arc::new(ltext), std::sync::Arc::new(rtext));
            }
        }
        let Some(c) = &self.compare else { return false };
        let (ltext, rtext) = (c.texts.0.clone(), c.texts.1.clone());
        let p = theme::palette(ui.ctx());
        let lname = self.name_of(&self.ws.tabs()[self.ws.position(left).unwrap_or(0)].editor);
        let rname = match &c.right {
            Side::Tab(id) => {
                self.name_of(&self.ws.tabs()[self.ws.position(*id).unwrap_or(0)].editor)
            }
            Side::Text { label, .. } => label.clone(),
        };
        let right_editable = matches!(c.right, Side::Tab(_));
        let changes = c.diff.hunks.len() as i64;
        let mut close = false;
        let mut ignore_ws = c.ignore_whitespace;
        egui::Frame::NONE
            .fill(p.surface)
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(format!("{lname}  ↔  {rname}"));
                    ui.colored_label(
                        p.muted,
                        self.t("compare-count", &[("count", changes.into())]),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(self.t("compare-close", &[])).clicked() {
                            close = true;
                        }
                        ui.checkbox(&mut ignore_ws, self.t("compare-ignore-whitespace", &[]));
                    });
                });
            });
        let labels = crate::diffview::Labels {
            to_left: self.t("compare-to-left", &[]),
            to_right: right_editable.then(|| self.t("compare-to-right", &[])),
        };
        let action = crate::diffview::show(
            ui,
            &c.diff,
            &ltext,
            &rtext,
            p,
            self.settings.font_size,
            &labels,
        );
        if close || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.compare = None;
            return true;
        }
        if let Some(c) = &mut self.compare
            && c.ignore_whitespace != ignore_ws
        {
            // Recompute on the next frame with the new option.
            c.ignore_whitespace = ignore_ws;
            c.revs = (Revision(u64::MAX), Revision(u64::MAX));
            ui.ctx().request_repaint();
            return true;
        }
        if let Some(a) = action {
            self.apply_hunk(a, &ltext, &rtext);
        }
        true
    }

    /// Copy one hunk across, unless either side changed since the diff was
    /// computed (then the comparison refreshes instead).
    fn apply_hunk(&mut self, a: crate::diffview::ApplyDir, ltext: &str, rtext: &str) {
        use crate::diffview::ApplyDir;
        let Some(c) = &self.compare else { return };
        let left = c.left;
        let right = match &c.right {
            Side::Tab(id) => Some(*id),
            Side::Text { .. } => None,
        };
        let now = (
            self.tab_text(left).map(|t| t.0),
            match right {
                Some(id) => self.tab_text(id).map(|t| t.0),
                None => Some(Revision(0)),
            },
        );
        if now != (Some(c.revs.0), Some(c.revs.1)) {
            self.status = Some((self.t("compare-stale", &[]), true));
            return;
        }
        let (target, bytes, text, target_text) = match a {
            ApplyDir::ToLeft(h) => {
                let Some(h) = c.diff.hunks.get(h) else { return };
                (
                    left,
                    h.left_bytes.clone(),
                    &rtext[h.right_bytes.clone()],
                    ltext,
                )
            }
            ApplyDir::ToRight(h) => {
                let Some(h) = c.diff.hunks.get(h) else { return };
                let Some(right) = right else { return };
                (
                    right,
                    h.right_bytes.clone(),
                    &ltext[h.left_bytes.clone()],
                    rtext,
                )
            }
        };
        let start = target_text[..bytes.start].chars().count();
        let end = start + target_text[bytes.clone()].chars().count();
        let text = text.to_owned();
        if let Some(i) = self.ws.position(target) {
            let current = self.ws.active_index();
            self.ws.activate(i);
            self.editor_mut().replace_range(start..end, &text);
            self.ws.activate(current);
        }
    }

    fn is_markdown(ed: &Editor) -> bool {
        ed.file()
            .and_then(|f| f.path.extension())
            .and_then(|e| e.to_str())
            .is_some_and(|e| {
                matches!(
                    e.to_ascii_lowercase().as_str(),
                    "md" | "markdown" | "mdown" | "mkd"
                )
            })
    }

    /// What a pane showing this tab draws, the same in either pane.
    fn pane_mode(&self, id: TabId) -> PaneContent {
        let Some(i) = self.ws.position(id) else {
            return PaneContent::Editor;
        };
        let ed = &self.ws.tabs()[i].editor;
        let structured = Self::data_kind(ed).is_some() && self.structured.contains(&id);
        let md = Self::is_markdown(ed)
            .then(|| self.md_modes.get(&id).copied().unwrap_or(MdMode::Source));
        pane_content(structured, md)
    }

    /// The content of one pane: `pane` is its slot (0 in a single area).
    /// Returns whether its editor was interacted with.
    fn pane_area(&mut self, ui: &mut egui::Ui, id: TabId, pane: usize) -> bool {
        match self.pane_mode(id) {
            PaneContent::Editor => self.editor_area(ui, id, pane),
            PaneContent::Preview => {
                self.preview_area(ui, id, pane);
                false
            }
            PaneContent::EditorAndPreview => {
                let mut touched = false;
                ui.columns(2, |cols| {
                    touched = self.editor_area(&mut cols[0], id, pane);
                    self.preview_area(&mut cols[1], id, pane);
                });
                touched
            }
            PaneContent::Structured => {
                self.structured_area(ui, id);
                // Tab past the grid's last cell: on to the next region (the
                // pane is drawn before it is listed, so it is the next one).
                if let Some(forward) = self.csv_grids.get_mut(&id).and_then(|g| g.leave.take()) {
                    // Shift+Tab before the first cell: the widget before
                    // the grid, as egui would.
                    if !forward {
                        self.focus.back_from = Some(grid_id(id));
                        ui.ctx().request_repaint();
                        return false;
                    }
                    let pane = Region::Pane(self.focus.now.panes.len());
                    let order = self.focus.order();
                    if let Some(r) = crate::focus::step(&order, Some(pane), forward) {
                        let entry = if forward {
                            FocusEntry::First
                        } else {
                            FocusEntry::Last
                        };
                        self.focus.go.get_or_insert((r, entry));
                    }
                }
                false
            }
        }
    }

    /// The widget that takes keyboard focus in a pane showing `id`.
    fn pane_entry(&self, id: TabId, slot: usize) -> Option<egui::Id> {
        match self.pane_mode(id) {
            PaneContent::Editor | PaneContent::EditorAndPreview => if slot == 0 {
                self.views.get(&id)
            } else {
                self.pane_views.get(&(slot, id))
            }
            .and_then(|v| v.widget_id),
            PaneContent::Preview => Some(preview_focus_id(id, slot)),
            PaneContent::Structured => {
                let ed = &self.ws.tabs()[self.ws.position(id)?].editor;
                Some(match Self::data_kind(ed) {
                    Some(DataKind::Csv | DataKind::Tsv) => grid_id(id),
                    _ => tree_id(id),
                })
            }
        }
    }

    /// Give the split pane in `slot` focus and make its tab active. Does
    /// nothing without a split.
    fn focus_pane(&mut self, slot: usize) {
        let Some(split) = &mut self.split else {
            return;
        };
        if let Some(i) = split.index_of_slot(slot)
            && i != split.focus()
        {
            split.set_focus(i);
            self.activate_focused_pane();
        }
    }

    /// Make the focused pane's tab the active tab.
    fn activate_focused_pane(&mut self) {
        let Some(split) = &self.split else {
            return;
        };
        if let Some(i) = self.ws.position(split.focused().tab) {
            self.ws.activate(i);
        }
    }

    /// Keep the split consistent with the tabs: the focused pane shows the
    /// active tab; a pane whose tab closed takes the active tab.
    fn sync_split(&mut self) {
        let active = self.ws.active().id;
        let Some(split) = &mut self.split else {
            return;
        };
        split.set_tab(split.focus(), active);
        for i in 0..split.len() {
            if split
                .get(i)
                .is_some_and(|p| self.ws.position(p.tab).is_none())
            {
                split.set_tab(i, active);
            }
        }
    }

    /// Add a pane next to the focused one. It starts with the tab after
    /// the active one, or the same document when only one is open; focus
    /// stays put.
    fn split_pane(&mut self, dir: Direction) {
        self.status = None;
        let active = self.ws.active().id;
        let i = self.ws.active_index();
        let other = self
            .ws
            .tabs()
            .get((i + 1) % self.ws.len())
            .map_or(active, |t| t.id);
        let split = self.split.get_or_insert_with(|| Panes::new(active));
        if !split.split(dir, other) {
            let msg = self.t("status-pane-limit", &[("max", MAX_PANES.into())]);
            self.status = Some((msg, false));
        }
        self.focus_active();
    }

    /// Close the focused split pane; the last one stays.
    fn close_pane(&mut self) {
        self.status = None;
        let Some(split) = &mut self.split else {
            return;
        };
        if let Some(closed) = split.close_focused()
            && closed.slot != 0
        {
            self.pane_views.retain(|(s, _), _| *s != closed.slot);
            self.md_synced.retain(|(_, s), _| *s != closed.slot);
        }
        self.activate_focused_pane();
        if self.split.as_ref().is_some_and(|s| !s.is_split()) {
            self.split = None;
        }
        self.focus_active();
    }

    /// Focus the next (or previous) split pane.
    fn cycle_pane(&mut self, forward: bool) {
        self.status = None;
        let Some(split) = &mut self.split else {
            return;
        };
        if forward {
            split.focus_next();
        } else {
            split.focus_prev();
        }
        self.activate_focused_pane();
        self.focus_active();
    }

    /// Move keyboard focus into `region`: a listed stop at once; a region
    /// whose widgets are not listed through its edge stop, before the
    /// next regions are drawn.
    fn go_region(&mut self, region: Region, entry: FocusEntry) {
        let ctx = self.ctx.clone();
        let d = &self.focus.last;
        let target = match region {
            Region::Tabs => match entry {
                FocusEntry::First => d.tabs.first().copied(),
                FocusEntry::Last => d.tabs.last().copied(),
                FocusEntry::Main => d.active_tab.or_else(|| d.tabs.first().copied()),
            },
            Region::Status => match entry {
                FocusEntry::Last => d.status.last().copied(),
                FocusEntry::First | FocusEntry::Main => d.status.first().copied(),
            },
            Region::Pane(n) => {
                let Some(pane) = d.panes.get(n).copied() else {
                    return;
                };
                self.focus_pane(pane.slot);
                // Tab: the pane's first widget (a grid's delimiter
                // buttons come before the grid); F6 and Shift+Tab: its
                // view.
                pane.entry.filter(|_| entry != FocusEntry::First)
            }
            Region::Find | Region::Outline | Region::Folder => None,
        };
        match target {
            Some(id) => ctx.memory_mut(|m| m.request_focus(id)),
            // Before anything is drawn (next frame, or this one when
            // called from the frame start).
            None => {
                self.focus.pending = Some((region, entry));
                ctx.request_repaint();
            }
        }
    }

    /// F6 / Shift+F6: the next (previous) region.
    fn cycle_region(&mut self, forward: bool) {
        let ctx = self.ctx.clone();
        let current = ctx
            .memory(|m| m.focused())
            .and_then(|f| self.focus.region_of(&ctx, f));
        if let Some(r) = crate::focus::step(&self.focus.order(), current, forward) {
            self.go_region(r, FocusEntry::Main);
        }
    }

    /// Act on focus noted by an edge stop this frame.
    fn take_focus_go(&mut self) {
        if let Some((r, e)) = self.focus.go.take() {
            self.go_region(r, e);
        }
    }

    /// Before anything is drawn: Tab through the tab strip and the status
    /// bar in their visual order and out of them to the neighbouring
    /// region, and Escape from them back to the document.
    fn focus_frame_start(&mut self, ctx: &egui::Context) {
        let tab = self.focus.note_tab(ctx);
        // Ctrl+Shift+Tab switches tabs; egui on macOS would also take it
        // as Shift+Tab (it ignores Control there).
        if ctx.input(|i| {
            i.events.iter().any(|e| {
                matches!(e, egui::Event::Key { key: egui::Key::Tab, pressed: true, modifiers, .. }
                    if modifiers.ctrl || modifiers.command || modifiers.alt)
            })
        }) {
            ctx.memory_mut(|m| m.move_focus(egui::FocusDirection::None));
        }
        if self.dialog.is_some() {
            return;
        }
        let focused = ctx.memory(|m| m.focused());
        if let (Some(forward), Some(f)) = (tab, focused) {
            let d = &self.focus.last;
            let found = [(Region::Tabs, &d.tabs), (Region::Status, &d.status)]
                .into_iter()
                .find_map(|(r, stops)| {
                    stops
                        .iter()
                        .position(|s| *s == f)
                        .map(|k| (r, k, stops.clone()))
                });
            if let Some((region, k, stops)) = found {
                // egui would follow creation order: take the key over.
                ctx.memory_mut(|m| m.move_focus(egui::FocusDirection::None));
                ctx.input_mut(|i| {
                    i.consume_key(egui::Modifiers::SHIFT, egui::Key::Tab)
                        || i.consume_key(egui::Modifiers::NONE, egui::Key::Tab)
                });
                match crate::focus::stop_step(stops.len(), k, forward) {
                    Some(j) => ctx.memory_mut(|m| m.request_focus(stops[j])),
                    None => {
                        let order = self.focus.order();
                        if let Some(next) = crate::focus::step(&order, Some(region), forward) {
                            let entry = if forward {
                                FocusEntry::First
                            } else {
                                FocusEntry::Last
                            };
                            self.go_region(next, entry);
                        }
                    }
                }
            }
        }
        let escape = ctx.input(|i| i.key_pressed(egui::Key::Escape) && i.modifiers.is_none());
        let d = &self.focus.last;
        if let Some(f) = self.focus.prev_focus
            && escape
            && (d.status.contains(&f) || d.tabs.contains(&f))
        {
            if self.focus.popup_was_open {
                // Escape closed the button's menu: stay on the button.
                ctx.memory_mut(|m| m.request_focus(f));
            } else {
                self.focus_active();
            }
        }
    }

    /// Up to four panes, side by side or stacked, each showing its tab as
    /// the single area would. Interacting with a pane makes its tab
    /// active, so menus and shortcuts act on it. Dragging a divider resizes
    /// the panes on both sides; double-clicking it shares the space evenly.
    fn split_area(&mut self, ui: &mut egui::Ui) -> bool {
        self.sync_split();
        let Some(split) = self.split.clone() else {
            return false;
        };
        let p = theme::palette(ui.ctx());
        let area = ui.available_rect_before_wrap();
        let gap = ui.spacing().item_spacing.x.max(4.0);
        let focus_slot = split.focused().slot;
        let count = split.len();
        let mut touched = None;
        for (n, (pane, f)) in split.panes().zip(split.fractions()).enumerate() {
            let (id, slot) = (pane.tab, pane.slot);
            if self.ws.position(id).is_none() {
                continue;
            }
            let x = |t: f32| area.left() + t * area.width();
            let y = |t: f32| area.top() + t * area.height();
            // Leave half a gap on each inner edge.
            let inner = |t: f32| if t > 0.0 && t < 1.0 { gap / 2.0 } else { 0.0 };
            let rect = egui::Rect::from_min_max(
                egui::pos2(x(f.x0) + inner(f.x0), y(f.y0) + inner(f.y0)),
                egui::pos2(x(f.x1) - inner(f.x1), y(f.y1) - inner(f.y1)),
            );
            let mut child = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(rect)
                    .layout(egui::Layout::top_down_justified(egui::Align::LEFT)),
            );
            child.set_clip_rect(rect.intersect(ui.clip_rect()));
            // For screen readers: a group per pane, named by its place and
            // tab, holding what the pane shows.
            let tab_name = self
                .ws
                .position(id)
                .map(|i| self.name_of(&self.ws.tabs()[i].editor))
                .unwrap_or_default();
            let args = [
                ("pane", ((n + 1) as i64).into()),
                ("count", (count as i64).into()),
                ("name", tab_name.into()),
            ];
            let pane_name = self.t(
                if slot == focus_slot {
                    "a11y-pane-active"
                } else {
                    "a11y-pane"
                },
                &args,
            );
            child
                .ctx()
                .accesskit_node_builder(child.unique_id(), |node| {
                    node.set_role(egui::accesskit::Role::Group);
                    node.set_label(pane_name);
                    node.set_bounds(egui::accesskit::Rect {
                        x0: rect.min.x.into(),
                        y0: rect.min.y.into(),
                        x1: rect.max.x.into(),
                        y1: rect.max.y.into(),
                    });
                });
            self.pane_ax = Some(self.t("a11y-pane-editor", &args));
            // A click anywhere in the pane (preview, tree) focuses it.
            let pressed =
                child.input(|i| i.pointer.any_pressed()) && child.rect_contains_pointer(rect);
            let k = self.focus.now.panes.len();
            let used = child
                .push_id((id, slot), |ui| {
                    self.focus.edge(ui, Region::Pane(k), Edge::Head);
                    let used = self.pane_area(ui, id, slot);
                    self.focus.edge(ui, Region::Pane(k), Edge::Tail);
                    used
                })
                .inner;
            self.focus.now.panes.push(crate::focus::PaneStop {
                slot,
                entry: self.pane_entry(id, slot),
                rect,
            });
            self.pane_ax = None;
            // Drawn after the content so a filled preview cannot hide it.
            if slot == focus_slot {
                ui.painter().hline(
                    rect.x_range(),
                    rect.top() + 1.0,
                    egui::Stroke::new(2.0, p.accent),
                );
            }
            if used || (pressed && self.dialog.is_none()) {
                touched = Some(slot);
            }
        }
        // Dividers last, so they sit above the panes' edges and win the
        // pointer there.
        let interactive = self.dialog.is_none();
        for d in split.dividers() {
            let (along, across) = if d.vertical {
                (area.y_range(), area.x_range())
            } else {
                (area.x_range(), area.y_range())
            };
            let at = across.min + d.at * across.span();
            let (from, to) = (
                along.min + d.from * along.span(),
                along.min + d.to * along.span(),
            );
            // Stop half a gap short of a crossing divider.
            let trim = |t: f32| if t > 0.0 && t < 1.0 { gap / 2.0 } else { 0.0 };
            let span = egui::Rangef::new(from + trim(d.from), to - trim(d.to));
            let hit = DIVIDER_HIT.max(gap);
            let hit_rect = if d.vertical {
                egui::Rect::from_x_y_ranges(egui::Rangef::new(at - hit / 2.0, at + hit / 2.0), span)
            } else {
                egui::Rect::from_x_y_ranges(span, egui::Rangef::new(at - hit / 2.0, at + hit / 2.0))
            };
            let sense = if interactive {
                egui::Sense::click_and_drag()
            } else {
                egui::Sense::hover()
            };
            let resp = ui.interact(
                hit_rect,
                ui.make_persistent_id(("pane-divider", d.id)),
                sense,
            );
            // Only while the button is held: should a release go missing,
            // the divider must not keep following the pointer.
            let dragging = resp.dragged() && ui.input(|i| i.pointer.primary_down());
            let active = interactive && (resp.hovered() || dragging);
            if active {
                ui.ctx().set_cursor_icon(if d.vertical {
                    egui::CursorIcon::ResizeHorizontal
                } else {
                    egui::CursorIcon::ResizeVertical
                });
            }
            if resp.double_clicked() {
                if let Some(s) = &mut self.split {
                    s.reset_divider(d.id);
                }
            } else if dragging
                && let Some(pos) = resp.interact_pointer_pos()
                && let Some(s) = &mut self.split
            {
                let extent = across.span().max(1.0);
                let t = ((if d.vertical { pos.x } else { pos.y }) - across.min) / extent;
                let min = (MIN_PANE_PX / extent).clamp(0.05, 0.5);
                s.move_divider(d.id, t, min);
                ui.ctx().request_repaint();
            }
            let stroke = if active {
                egui::Stroke::new(2.0, p.accent)
            } else {
                egui::Stroke::new(1.0, p.border)
            };
            if d.vertical {
                ui.painter().vline(at, span, stroke);
            } else {
                ui.painter().hline(span, at, stroke);
            }
        }
        ui.advance_cursor_after_rect(area);
        if let Some(slot) = touched {
            self.status = None;
            self.focus_pane(slot);
        }
        true
    }

    /// The text editor of a pane. Returns whether it was interacted with.
    fn editor_area(&mut self, ui: &mut egui::Ui, id: TabId, pane: usize) -> bool {
        let name = self
            .pane_ax
            .clone()
            .unwrap_or_else(|| self.t("editor-accessible-name", &[]));
        let menu = self.context_labels();
        let interactive = self.dialog.is_none();
        let (indent, tab_width) = self.tab_indent(id);
        let active = id == self.ws.active().id;
        let Some(i) = self.ws.position(id) else {
            return false;
        };
        // Field borrows, not `slot_view`, so the tab can be borrowed too.
        let view = if pane == 0 {
            self.views.entry(id).or_default()
        } else {
            self.pane_views.entry((pane, id)).or_default()
        };
        let Some(tab) = self.ws.tab_mut(i) else {
            return false;
        };
        let editor = &mut tab.editor;
        // Spans from an older revision are kept until fresh ones arrive;
        // a frame of slightly shifted colour beats flicker.
        let syntax: &[CharSpan] = self.spans.get(&id).map_or(&[], |(_, s)| s);
        let opts = ViewOptions {
            font_size: self.settings.font_size,
            indent,
            tab_width,
            wrap: self.settings.word_wrap,
            menu,
        };
        let highlights: &[std::ops::Range<usize>] = if self.find.open && active {
            &self.find.matches.ranges
        } else {
            &[]
        };
        let mut touched = false;
        ui.push_id(id, |ui| {
            ui.add_enabled_ui(interactive, |ui| {
                touched = view.show(ui, editor, &name, highlights, syntax, &opts);
            });
        });
        if touched {
            self.status = None;
        }
        touched
    }

    /// The Markdown preview of a pane's tab, following that tab's caret.
    fn preview_area(&mut self, ui: &mut egui::Ui, id: TabId, pane: usize) {
        let Some(i) = self.ws.position(id) else {
            return;
        };
        let tab = &self.ws.tabs()[i];
        let rev = tab.editor.document().revision();
        let doc = tab.editor.document();
        let p = theme::palette(ui.ctx());
        if doc.rope().len_bytes() > preview::MAX_PREVIEW_BYTES {
            ui.colored_label(p.muted, self.t("preview-too-large", &[]));
            return;
        }
        let fresh = self.md_cache.get(&id).is_some_and(|(r, _)| *r == rev);
        if !fresh {
            let blocks = text3_markdown::parse(&doc.text());
            self.md_cache.insert(id, (rev, std::sync::Arc::new(blocks)));
        }
        let Some((_, blocks)) = self.md_cache.get(&id) else {
            return;
        };
        let blocks = blocks.clone();
        let ed = &self.ws.tabs()[i].editor;
        let head = ed.selections().primary().head;
        let caret_line = text3_core::selection::line_col(ed.document().rope(), head).0 + 1;
        let scroll = self.md_synced.get(&(id, pane)) != Some(&caret_line);
        // An anchor click scrolls once, on the next frame, in the focused
        // pane (the one clicked).
        let focused = self.split.as_ref().is_none_or(|s| s.focused().slot == pane);
        let anchor = if focused {
            self.md_anchor.remove(&id)
        } else {
            None
        };
        let mut event = None;
        self.md_synced.insert((id, pane), caret_line);
        let l10n = &self.l10n;
        let untranslated = |key: &str| tr(l10n, key, &[]);
        let doc_dir = ed
            .file()
            .and_then(|f| f.path.parent().map(std::path::Path::to_path_buf));
        let ctx = ui.ctx().clone();
        self.ctx = ctx.clone();
        let images = &self.images;
        let allowed = &self.allowed_image_dirs;
        let image = |url: &str| {
            use crate::imagecache::ImageView;
            use text3_app::images::ImageSource;
            match text3_app::images::resolve(url, doc_dir.as_deref()) {
                ImageSource::Local(path) => images.borrow_mut().get(&ctx, &path),
                ImageSource::Outside(path) if allowed.iter().any(|d| path.starts_with(d)) => {
                    images.borrow_mut().get(&ctx, &path)
                }
                ImageSource::Outside(path) => {
                    ImageView::Ask(path.parent().map(Path::to_path_buf).unwrap_or(path))
                }
                ImageSource::Remote => ImageView::Note("image-remote"),
                ImageSource::Blocked => ImageView::Note("image-blocked"),
            }
        };
        self.images.borrow_mut().poll(&ctx);
        let format = |id: &str, args: &[(&str, FluentValue<'_>)]| tr(l10n, id, args);
        let style = preview::Style {
            p,
            size: self.settings.font_size + 1.0,
            untranslated: &untranslated,
            format: &format,
            image: &image,
        };
        // Focusable as a whole (Tab, F6), so a preview-only pane has a
        // keyboard stop; links inside follow it.
        let region = egui::UiBuilder::new()
            .scope_id(preview_focus_id(id, pane))
            .sense(egui::Sense::focusable_noninteractive())
            .accessibility_role(egui::accesskit::Role::Document)
            .accessibility_label(tr(l10n, "view-markdown-preview", &[]));
        ui.scope_builder(region, |ui| {
            egui::Frame::NONE
                .fill(p.canvas)
                .inner_margin(egui::Margin::symmetric(24, 16))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt(("md-preview", id))
                        .auto_shrink(false)
                        .show(ui, |ui| {
                            ui.set_max_width(760.0_f32.min(ui.available_width()));
                            event = preview::show(
                                ui,
                                &blocks,
                                &style,
                                caret_line,
                                scroll,
                                anchor.as_deref(),
                            );
                        });
                });
        });
        if event.is_some() {
            // Links resolve against the active tab: the clicked pane's.
            self.focus_pane(pane);
        }
        match event {
            Some(preview::Event::LinkClicked(url)) => self.follow_link(&url),
            Some(preview::Event::AllowImages(dir)) => self.allowed_image_dirs.push(dir),
            None => {}
        }
    }

    /// Act on a link clicked in the preview. Web links need confirmation;
    /// local files open in a tab; anchors scroll; everything else is refused.
    fn follow_link(&mut self, url: &str) {
        use text3_markdown::LinkKind;
        match text3_markdown::classify_link(url) {
            LinkKind::External => self.dialog = Some(Dialog::OpenLink(url.trim().to_owned())),
            LinkKind::Anchor => {
                let id = self.ws.active().id;
                self.md_anchor
                    .insert(id, url.trim().trim_start_matches('#').to_owned());
            }
            LinkKind::Relative => {
                let target = url
                    .split(['#', '?'])
                    .next()
                    .unwrap_or("")
                    .replace("%20", " ");
                let base = self
                    .editor()
                    .file()
                    .and_then(|f| f.path.parent().map(std::path::Path::to_path_buf));
                match base.map(|b| b.join(&target)).filter(|p| p.is_file()) {
                    Some(path) => self.open(&path),
                    None => {
                        let msg = self.t("link-not-found", &[("target", target.into())]);
                        self.status = Some((msg, true));
                    }
                }
            }
            LinkKind::Blocked => self.status = Some((self.t("link-blocked", &[]), true)),
        }
    }

    /// Language of a tab: the user's choice, else the file name, else the
    /// first line of the text.
    fn language(&self, id: TabId, ed: &Editor) -> Option<Lang> {
        if let Some(choice) = self.lang_override.get(&id) {
            return *choice;
        }
        ed.file()
            .and_then(|f| Lang::from_path(&f.path))
            .or_else(|| {
                let rope = ed.document().rope();
                let end = rope.len_chars().min(512);
                Lang::from_content(&rope.slice(..end).to_string())
            })
    }

    /// Ask for highlighting of the active tab if its text changed, and
    /// collect finished results.
    fn update_syntax(&mut self, ctx: &egui::Context) {
        for h in self.syntax.poll() {
            if let Some(t) = self.ws.position(h.tab).and_then(|i| self.ws.tab_mut(i)) {
                t.editor.set_syntax_folds(h.revision, h.folds);
            }
            self.spans.insert(h.tab, (h.revision, h.spans));
        }
        let tab = self.ws.active();
        let rev = tab.editor.document().revision();
        let lang = self.language(tab.id, &tab.editor);
        // Without a syntax tree, folding goes by indentation.
        if lang.is_none() || tab.editor.document().rope().len_bytes() > MAX_BYTES {
            self.editor_mut().clear_syntax_folds();
        }
        let tab = self.ws.active();
        match lang {
            Some(lang) if self.requested.get(&tab.id) != Some(&rev) => {
                self.syntax
                    .submit(tab.id, rev, lang, tab.editor.document().rope().clone());
                self.requested.insert(tab.id, rev);
            }
            None => {
                self.spans.remove(&tab.id);
            }
            _ => {}
        }
        // Results arrive from another thread; check back shortly while
        // the active tab is waiting for fresh spans.
        let fresh = self.spans.get(&tab.id).is_some_and(|(r, _)| *r == rev);
        if lang.is_some() && !fresh {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }

    fn enabled(&self, action: MenuAction) -> bool {
        let doc = self.editor().document();
        self.dialog.is_none()
            && match action {
                MenuAction::Undo => doc.can_undo(),
                MenuAction::Redo => doc.can_redo(),
                MenuAction::Cut | MenuAction::Copy => self.editor().copy().is_some(),
                MenuAction::NextTab | MenuAction::PrevTab => self.ws.len() > 1,
                MenuAction::MoveTabLeft | MenuAction::MoveTabRight => {
                    let i = self.ws.active_index();
                    let to = if matches!(action, MenuAction::MoveTabLeft) {
                        i.checked_sub(1)
                    } else {
                        Some(i + 1).filter(|&t| t < self.ws.len())
                    };
                    // Pinned and unpinned tabs stay in their own groups.
                    to.is_some_and(|t| self.ws.tabs()[t].pinned == self.ws.tabs()[i].pinned)
                }
                MenuAction::MarkdownSource
                | MenuAction::MarkdownSplit
                | MenuAction::MarkdownPreview => Self::is_markdown(self.editor()),
                MenuAction::StructuredView => Self::data_kind(self.editor()).is_some(),
                MenuAction::ReopenWithEncoding => self.editor().file().is_some(),
                MenuAction::LocalHistory => {
                    self.editor().file().is_some() && self.history.is_some()
                }
                MenuAction::ClearHistory => self.history.is_some(),
                MenuAction::GoToHeading => Self::is_markdown(self.editor()),
                MenuAction::CompareSaved
                | MenuAction::Revert
                | MenuAction::CopyPath
                | MenuAction::Reveal => self.editor().file().is_some(),
                MenuAction::PlayMacro | MenuAction::PlayMacroTimes => {
                    !self.macro_cmds.is_empty() && !self.editor().is_recording()
                }
                MenuAction::SaveMacro | MenuAction::PlaySavedMacro => !self.editor().is_recording(),
                MenuAction::PinTab => !self.ws.active().pinned,
                MenuAction::UnpinTab => self.ws.active().pinned,
                _ => true,
            }
    }

    /// Whether a text field other than the editor has keyboard focus. The
    /// CSV grid's own focus (not its cell editor) counts as the editor, so
    /// undo and redo reach the document.
    fn text_field_focused(&self, ctx: &egui::Context) -> bool {
        let active = self.ws.active().id;
        let editor = self.views.get(&active).and_then(|v| v.widget_id);
        ctx.memory(|m| m.focused())
            .is_some_and(|f| Some(f) != editor && f != grid_id(active) && f != tree_id(active))
    }

    fn dispatch(&mut self, action: MenuAction, ctx: &egui::Context) {
        // Native menu accelerators can bypass egui's raw key events.
        for view in self.views.values_mut().chain(self.pane_views.values_mut()) {
            view.end_column_mode();
        }
        if self.dialog.is_some() {
            return;
        }
        // Menu shortcuts reach us even when a field like the find bar has
        // focus: hand clipboard and undo there instead of to the document.
        if self.text_field_focused(ctx) {
            use egui::{Event, Key, Modifiers};
            let key = |key, modifiers| Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            };
            let event = match action {
                MenuAction::Copy => Some(Event::Copy),
                MenuAction::Cut => Some(Event::Cut),
                MenuAction::Paste => arboard::Clipboard::new()
                    .and_then(|mut c| c.get_text())
                    .ok()
                    .map(Event::Paste),
                MenuAction::SelectAll => Some(key(Key::A, Modifiers::COMMAND)),
                MenuAction::Undo => Some(key(Key::Z, Modifiers::COMMAND)),
                MenuAction::Redo => Some(key(Key::Z, Modifiers::COMMAND.plus(Modifiers::SHIFT))),
                _ => None,
            };
            if let Some(e) = event {
                ctx.input_mut(|i| i.events.push(e));
                return;
            }
        }
        let editor_cmd = |cmd| Some(cmd);
        let (tab_indent, tab_width) = self.tab_indent(self.ws.active().id);
        let cmd = match action {
            MenuAction::New => {
                self.ws.new_tab();
                None
            }
            MenuAction::Open => {
                self.open_dialog();
                None
            }
            MenuAction::Save => {
                self.save();
                None
            }
            MenuAction::SaveAs => {
                self.save_as();
                None
            }
            MenuAction::CloseTab => {
                let id = self.ws.active().id;
                self.request_close_tab(id, ctx);
                None
            }
            MenuAction::ReopenTab => {
                if let Some(Err(e)) = self.ws.reopen_closed() {
                    let path = match &e {
                        LoadError::Io { path, .. } | LoadError::Decode { path, .. } => path.clone(),
                    };
                    self.dialog = Some(Dialog::Error(describe_load_error(&self.l10n, &path, &e)));
                } else {
                    self.focus_active();
                }
                None
            }
            MenuAction::NextTab => {
                self.ws.cycle(true);
                None
            }
            MenuAction::PrevTab => {
                self.ws.cycle(false);
                None
            }
            MenuAction::MoveTabLeft | MenuAction::MoveTabRight => {
                let i = self.ws.active_index();
                let to = if matches!(action, MenuAction::MoveTabLeft) {
                    i.saturating_sub(1)
                } else {
                    i + 1
                };
                self.ws.move_tab(i, to);
                None
            }
            // Goes through close_requested, which guards unsaved changes.
            MenuAction::Quit => {
                ctx.send_viewport_cmd(ViewportCommand::Close);
                None
            }
            MenuAction::Find | MenuAction::Replace => {
                let seed = self.editor().copy();
                self.find.open(action == MenuAction::Replace, seed);
                return;
            }
            MenuAction::FindNext | MenuAction::FindPrev => {
                if !self.find.open {
                    self.find.open(false, None);
                }
                self.find_step(action == MenuAction::FindNext);
                return;
            }
            MenuAction::MarkdownSource
            | MenuAction::MarkdownSplit
            | MenuAction::MarkdownPreview => {
                if Self::is_markdown(self.editor()) {
                    let mode = match action {
                        MenuAction::MarkdownSplit => MdMode::Split,
                        MenuAction::MarkdownPreview => MdMode::Preview,
                        _ => MdMode::Source,
                    };
                    self.md_modes.insert(self.ws.active().id, mode);
                }
                None
            }
            MenuAction::CommandPalette => {
                self.palette.open(crate::palette::Mode::Commands);
                return;
            }
            MenuAction::PinTab | MenuAction::UnpinTab => {
                let i = self.ws.active_index();
                self.ws.set_pinned(i, action == MenuAction::PinTab);
                return;
            }
            MenuAction::OpenLog => {
                if let Some(p) = rfd::FileDialog::new().pick_file() {
                    self.open_log(&p, ctx);
                }
                return;
            }
            MenuAction::LocalHistory => {
                self.history_open = self.editor().file().is_some();
                return;
            }
            MenuAction::ClearHistory => {
                self.dialog = Some(Dialog::ConfirmPurge);
                return;
            }
            MenuAction::CheckUpdates => {
                if !text3_app::STORE {
                    self.start_update_check(ctx, true);
                }
                return;
            }
            MenuAction::FindInFolder => {
                let dir = self
                    .editor()
                    .file()
                    .and_then(|f| f.path.parent().map(std::path::Path::to_path_buf));
                self.folder.show_panel(dir);
                if self.folder.root.is_none() {
                    self.folder.root = rfd::FileDialog::new().pick_folder();
                }
                return;
            }
            MenuAction::Fold => {
                let tab = usize::from(self.settings.tab_width);
                self.editor_mut().fold_at_caret(tab);
                None
            }
            MenuAction::Unfold => {
                self.editor_mut().unfold_at_caret();
                None
            }
            MenuAction::UnfoldAll => {
                self.editor_mut().unfold_all();
                None
            }
            MenuAction::ReopenWithEncoding => {
                match self.editor().file().map(|f| f.path.clone()) {
                    Some(_) if self.editor().document().is_dirty() => {
                        self.status = Some((self.t("encoding-save-first", &[]), true));
                    }
                    Some(path) => self.start_reopen(&path),
                    None => {}
                }
                None
            }
            MenuAction::SaveWithEncoding => {
                let current = self.editor().encoding();
                let selected = encoding_choices(false)
                    .iter()
                    .position(|e| *e == current)
                    .unwrap_or(0);
                self.encoding_pick = Some(EncodingPick::Save {
                    selected,
                    error: None,
                });
                None
            }
            MenuAction::StructuredView => {
                if Self::data_kind(self.editor()).is_some() {
                    let id = self.ws.active().id;
                    if !self.structured.remove(&id) {
                        self.structured.insert(id);
                    }
                }
                None
            }
            MenuAction::CompareTabs => {
                if self.compare.take().is_none() {
                    match self.ws.len() {
                        0 | 1 => self.status = Some((self.t("compare-need-two", &[]), false)),
                        2 => {
                            let other = self.ws.tabs()[1 - self.ws.active_index()].id;
                            self.start_compare(Side::Tab(other));
                        }
                        _ => self.compare_pick = true,
                    }
                }
                None
            }
            MenuAction::JsonValidate
            | MenuAction::JsonFormat
            | MenuAction::JsonMinify
            | MenuAction::SortLines
            | MenuAction::SortLinesDesc
            | MenuAction::UniqueLines
            | MenuAction::TrimTrailing
            | MenuAction::UpperCase
            | MenuAction::LowerCase
            | MenuAction::ToLf
            | MenuAction::ToCrLf => {
                self.run_tool(action);
                None
            }
            MenuAction::Settings => {
                self.settings_open = true;
                self.settings_focus = true;
                return;
            }
            MenuAction::CloseOthers => {
                let id = self.ws.active().id;
                self.close_many(Some(id), ctx);
                return;
            }
            MenuAction::CloseAll => {
                self.close_many(None, ctx);
                return;
            }
            MenuAction::CopyPath => {
                let id = self.ws.active().id;
                self.copy_path(id);
                return;
            }
            MenuAction::Reveal => {
                let id = self.ws.active().id;
                self.reveal(id);
                return;
            }
            MenuAction::OpenRecent => {
                self.palette.open(crate::palette::Mode::Recent);
                return;
            }
            MenuAction::SaveAll => {
                self.save_all();
                return;
            }
            MenuAction::Revert => {
                if self.editor().document().is_dirty() {
                    self.dialog = Some(Dialog::ConfirmRevert);
                } else {
                    self.revert();
                }
                return;
            }
            MenuAction::XmlValidate | MenuAction::YamlValidate => {
                let text = self.editor().document().text();
                let (format, result) = if action == MenuAction::XmlValidate {
                    ("format-xml", text3_tools::markup::xml_tree(&text).map(drop))
                } else {
                    (
                        "format-yaml",
                        text3_tools::markup::yaml_tree(&text).map(drop),
                    )
                };
                let format = self.t(format, &[]);
                match result {
                    Ok(()) => {
                        let msg = self.t("tools-markup-valid", &[("format", format.into())]);
                        self.status = Some((msg, false));
                    }
                    Err(e) => {
                        // Put the caret on the problem.
                        self.editor_mut()
                            .go_to(e.line.saturating_sub(1), e.column.saturating_sub(1));
                        let id = self.ws.active().id;
                        self.active_view(id).reveal();
                        let msg = self.t(
                            "tools-tree-error",
                            &[
                                ("format", format.into()),
                                ("line", (e.line as i64).into()),
                                ("column", (e.column as i64).into()),
                                ("reason", e.message.into()),
                            ],
                        );
                        self.status = Some((msg, true));
                    }
                }
                return;
            }
            MenuAction::XmlFormat => {
                let range = self.editor().tool_range();
                let result = self
                    .editor_mut()
                    .transform(|t| text3_tools::markup::format_xml(t, &tab_indent));
                match result {
                    Ok(changed) => {
                        let id = if changed {
                            "tools-done"
                        } else {
                            "tools-no-change"
                        };
                        self.status = Some((self.t(id, &[]), false));
                    }
                    Err(e) => {
                        // Put the caret on the problem; the position is
                        // relative to the formatted range.
                        let rope = self.editor().document().rope();
                        let first = rope.char_to_line(range.start);
                        let line = first + e.line.saturating_sub(1);
                        let mut column = e.column.saturating_sub(1);
                        if e.line <= 1 {
                            column += range.start - rope.line_to_char(first);
                        }
                        self.editor_mut().go_to(line, column);
                        let id = self.ws.active().id;
                        self.active_view(id).reveal();
                        let format = self.t("format-xml", &[]);
                        let msg = self.t(
                            "tools-tree-error",
                            &[
                                ("format", format.into()),
                                ("line", ((line + 1) as i64).into()),
                                ("column", ((column + 1) as i64).into()),
                                ("reason", e.message.into()),
                            ],
                        );
                        self.status = Some((msg, true));
                    }
                }
                None
            }
            MenuAction::About => {
                self.about_open = true;
                return;
            }
            MenuAction::SplitEditor => {
                self.split_pane(Direction::Right);
                return;
            }
            MenuAction::SplitDown => {
                self.split_pane(Direction::Down);
                return;
            }
            MenuAction::ClosePane => {
                self.close_pane();
                return;
            }
            MenuAction::FocusNextPane => {
                self.cycle_pane(true);
                return;
            }
            MenuAction::FocusPrevPane => {
                self.cycle_pane(false);
                return;
            }
            MenuAction::FocusNextRegion => {
                self.cycle_region(true);
                return;
            }
            MenuAction::FocusPrevRegion => {
                self.cycle_region(false);
                return;
            }
            MenuAction::CompareSaved => {
                let Some(path) = self.editor().file().map(|f| f.path.clone()) else {
                    return;
                };
                let enc = self.editor().encoding();
                let text = std::fs::read(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|b| text3_io::decode_as(&b, enc).map_err(|e| e.to_string()));
                match text {
                    Ok(text) => {
                        let label = self.t(
                            "compare-saved-label",
                            &[("name", self.display_name().into())],
                        );
                        self.start_compare(Side::Text { label, text });
                    }
                    Err(reason) => {
                        self.status = Some((
                            self.t("compare-saved-failed", &[("reason", reason.into())]),
                            true,
                        ));
                    }
                }
                return;
            }
            MenuAction::CompareClipboard => {
                match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
                    Ok(text) => {
                        let label = self.t("compare-clipboard-label", &[]);
                        self.start_compare(Side::Text { label, text });
                    }
                    Err(_) => self.status = Some((self.t("compare-clipboard-empty", &[]), true)),
                }
                return;
            }
            MenuAction::CompareFile => {
                let Some(path) = rfd::FileDialog::new().pick_file() else {
                    return;
                };
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                match text3_io::load(&path) {
                    Ok(loaded) => self.start_compare(Side::Text {
                        label: name,
                        text: loaded.text,
                    }),
                    Err(e) => {
                        self.status = Some((
                            self.t(
                                "compare-file-failed",
                                &[("name", name.into()), ("reason", e.to_string().into())],
                            ),
                            true,
                        ));
                    }
                }
                return;
            }
            MenuAction::RecordMacro => {
                if self.editor().is_recording() {
                    self.macro_cmds = self.editor_mut().stop_recording();
                    let n = self.macro_cmds.len() as i64;
                    self.status = Some((self.t("macro-recorded", &[("count", n.into())]), false));
                } else {
                    self.editor_mut().start_recording();
                    self.status = None;
                }
                return;
            }
            MenuAction::PlayMacro => {
                let cmds = self.macro_cmds.clone();
                self.editor_mut().play(&cmds);
                let id = self.ws.active().id;
                self.active_view(id).reveal();
                self.focus_active();
                return;
            }
            MenuAction::GoToLine => {
                self.palette.open(crate::palette::Mode::GoToLine);
                return;
            }
            MenuAction::PlayMacroTimes => {
                self.palette.open(crate::palette::Mode::MacroTimes);
                return;
            }
            MenuAction::SaveMacro => {
                if self.macro_cmds.is_empty() {
                    self.status = Some((self.t("macro-save-empty", &[]), true));
                } else {
                    self.palette.open(crate::palette::Mode::MacroSave);
                }
                return;
            }
            MenuAction::PlaySavedMacro => {
                self.palette.open(crate::palette::Mode::MacroPlaySaved);
                return;
            }
            MenuAction::DeleteSavedMacro => {
                self.palette.open(crate::palette::Mode::MacroDeleteSaved);
                return;
            }
            MenuAction::GoToHeading => {
                self.palette.open(crate::palette::Mode::Headings);
                return;
            }
            MenuAction::GotoAnything => {
                self.start_goto();
                return;
            }
            MenuAction::Outline => {
                self.outline_open = !self.outline_open;
                self.outline_follow = true;
                return;
            }
            MenuAction::WordWrap => {
                self.settings.word_wrap = !self.settings.word_wrap;
                self.save_settings();
                let id = if self.settings.word_wrap {
                    "status-wrap-on"
                } else {
                    "status-wrap-off"
                };
                self.status = Some((self.t(id, &[]), false));
                return;
            }
            MenuAction::ZoomIn | MenuAction::ZoomOut | MenuAction::ZoomReset => {
                match action {
                    MenuAction::ZoomIn => self.settings.zoom(1.0),
                    MenuAction::ZoomOut => self.settings.zoom(-1.0),
                    _ => self.settings.font_size = FONT_SIZE_DEFAULT,
                }
                self.save_settings();
                return;
            }
            MenuAction::Undo => editor_cmd(Command::Undo),
            MenuAction::Redo => editor_cmd(Command::Redo),
            MenuAction::SelectAll => editor_cmd(Command::SelectAll),
            MenuAction::DuplicateLine => editor_cmd(Command::DuplicateLine),
            MenuAction::IndentLines => editor_cmd(Command::IndentLines(tab_indent)),
            MenuAction::OutdentLines => editor_cmd(Command::OutdentLines(tab_width)),
            MenuAction::ToggleComment => {
                match self
                    .language(self.ws.active().id, self.editor())
                    .and_then(|l| l.line_comment())
                {
                    Some(prefix) => editor_cmd(Command::ToggleComment(prefix.to_owned())),
                    None => {
                        self.status = Some((self.t("comment-unsupported", &[]), false));
                        None
                    }
                }
            }
            MenuAction::SelectNextOccurrence => editor_cmd(Command::SelectNextOccurrence),
            MenuAction::SkipOccurrence => editor_cmd(Command::SkipOccurrence),
            MenuAction::AddCursorAbove => editor_cmd(Command::AddCaretVertical { down: false }),
            MenuAction::AddCursorBelow => editor_cmd(Command::AddCaretVertical { down: true }),
            MenuAction::DeleteLine => editor_cmd(Command::DeleteLine),
            MenuAction::MoveLineUp => editor_cmd(Command::MoveLineUp),
            MenuAction::MoveLineDown => editor_cmd(Command::MoveLineDown),
            MenuAction::Copy => {
                if let Some(t) = self.editor().copy() {
                    ctx.copy_text(t);
                }
                None
            }
            MenuAction::Cut => {
                if let Some(t) = self.editor_mut().execute(Command::Cut) {
                    ctx.copy_text(t);
                }
                None
            }
            MenuAction::Paste => arboard::Clipboard::new()
                .and_then(|mut c| c.get_text())
                .ok()
                .filter(|t| !t.is_empty())
                .map(Command::Insert),
        };
        if let Some(cmd) = cmd {
            self.editor_mut().execute(cmd);
        }
        self.focus_active();
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let p = theme::palette(ctx);
        let mut activate = None;
        let mut close = None;
        let mut pin: Option<(usize, bool)> = None;
        let mut tab_cmd: Option<(TabId, MenuAction)> = None;
        let (close_others_l, close_all_l, copy_path_l, reveal_l) = (
            self.t("tab-close-others", &[]),
            self.t("tab-close-all", &[]),
            self.t("file-copy-path", &[]),
            self.t(crate::menu::REVEAL_LABEL, &[]),
        );
        let mut rects: Vec<egui::Rect> = Vec::new();
        let mut dragging: Option<usize> = None;
        let mut dropped: Option<usize> = None;
        let (pin_l, unpin_l, close_l) = (
            self.t("tab-pin", &[]),
            self.t("tab-unpin", &[]),
            self.t("file-close-tab", &[]),
        );
        let (strip_l, close_tab_l) = (self.t("a11y-tab-strip", &[]), self.t("tab-close", &[]));
        let missing_l = self.t("tab-missing-on-disk", &[]);
        let new_tab_l = self.t(MenuAction::New.label_id(), &[]);
        // The tooltip's shortcut symbols (⌘, ⌥) are formatted only when it
        // shows: `format_shortcut` checks the font has them, and ⌥ is not
        // in the built-in fonts, so on the first frame it would wait for
        // the system font list (20–35 ms on macOS). The accessible
        // description is not drawn, so it needs no glyph check.
        let new_tab_key = MenuAction::New.shortcut(&self.settings.keymap);
        let new_tab_desc = new_tab_key.map(|s| {
            let mac = ctx.os().is_mac();
            let names = if mac {
                egui::ModifierNames::SYMBOLS
            } else {
                egui::ModifierNames::NAMES
            };
            format!("{new_tab_l} ({})", s.format(&names, mac))
        });
        // Keyboard stops: each tab with its close button, then "+".
        let mut tab_stops: Vec<(egui::Id, Option<egui::Id>)> = Vec::new();
        let mut active_stop = None;
        let mut new_tab = false;
        let mut in_view = self.tab_in_view;
        // The "+" button follows the last tab; when the tabs overflow, the
        // scrolling part stops short of it so it stays at the strip's end.
        const NEW_TAB_W: f32 = 28.0;
        let outer = ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let scroll = egui::ScrollArea::horizontal()
                .max_width((ui.available_width() - NEW_TAB_W).max(0.0))
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.ctx().accesskit_node_builder(ui.unique_id(), |n| {
                            n.set_role(egui::accesskit::Role::TabList);
                            n.set_label(strip_l.as_str());
                        });
                        ui.spacing_mut().item_spacing.x = 0.0;
                        let active = self.ws.active_index();
                        for (i, tab) in self.ws.tabs().iter().enumerate() {
                            let is_active = i == active;
                            let dirty = tab.editor.document().is_dirty();
                            let name = self.name_of(&tab.editor);
                            let missing = tab.editor.is_missing();
                            let mut ax_name = if dirty {
                                self.t("a11y-tab-unsaved", &[("name", name.as_str().into())])
                            } else {
                                name.clone()
                            };
                            if missing {
                                ax_name = format!("{ax_name}, {missing_l}");
                            }
                            let frame = egui::Frame::NONE
                                .fill(if is_active { p.canvas } else { p.surface })
                                .inner_margin(egui::Margin {
                                    left: 12,
                                    right: 6,
                                    top: 5,
                                    bottom: 5,
                                });
                            // Where the close button is: the tab-wide click/drag
                            // area below is registered later and wins egui's hit
                            // test, so clicks on × are matched by position.
                            let mut close_rect: Option<egui::Rect> = None;
                            let mut close_id = None;
                            let r = frame.show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 6.0;
                                    let color = if is_active { p.text } else { p.muted };
                                    // Clicks go to the whole tab (below). A file
                                    // that is gone is struck through.
                                    let mut text = egui::RichText::new(&name).color(color);
                                    if missing {
                                        text = text.strikethrough();
                                    }
                                    let label = ui.add(egui::Label::new(text).selectable(false));
                                    if missing {
                                        label.on_hover_text(missing_l.as_str());
                                    }
                                    if tab.pinned && !dirty {
                                        // Pinned tabs show a pin, not a close
                                        // button (closing stays in the menus).
                                        let (r, painter) = ui.allocate_painter(
                                            egui::vec2(16.0, 16.0),
                                            egui::Sense::hover(),
                                        );
                                        let c = r.rect.center();
                                        let stroke = egui::Stroke::new(1.5, p.muted);
                                        painter.circle_filled(
                                            c - egui::vec2(0.0, 3.0),
                                            3.0,
                                            p.muted,
                                        );
                                        painter.line_segment([c, c + egui::vec2(0.0, 6.0)], stroke);
                                        return;
                                    }
                                    // Unsaved marker doubles as the close button,
                                    // as in most native editors.
                                    let glyph = if dirty { "●" } else { "×" };
                                    let btn = ui
                                        .add(
                                            egui::Button::new(
                                                egui::RichText::new(glyph).color(p.muted),
                                            )
                                            .frame(false)
                                            .min_size(egui::vec2(16.0, 16.0)),
                                        )
                                        .on_hover_text(close_tab_l.as_str());
                                    ui.ctx().accesskit_node_builder(btn.id, |n| {
                                        n.set_label(close_tab_l.as_str());
                                    });
                                    close_rect = Some(btn.rect);
                                    close_id = Some(btn.id);
                                    if btn.clicked() {
                                        close = Some(tab.id);
                                    }
                                });
                            });
                            let rect = r.response.rect;
                            let painter = ui.painter();
                            painter.vline(
                                rect.right(),
                                rect.y_range(),
                                egui::Stroke::new(1.0, p.border),
                            );
                            if is_active {
                                let top = egui::Rect::from_min_size(
                                    rect.min,
                                    egui::vec2(rect.width(), 2.0),
                                );
                                painter.rect_filled(top, 0.0, p.accent);
                            } else {
                                painter.hline(
                                    rect.x_range(),
                                    rect.bottom() - 0.5,
                                    egui::Stroke::new(1.0, p.border),
                                );
                            }
                            rects.push(rect);
                            if is_active && in_view != Some(tab.id) {
                                ui.scroll_to_rect(rect, None);
                                in_view = Some(tab.id);
                            }
                            // A widget of its own over the tab: the frame's id is
                            // re-registered without focus every frame, which
                            // would drop keyboard focus.
                            let r = ui.interact(
                                rect,
                                egui::Id::unique(("text3-tab", tab.id)),
                                egui::Sense::click_and_drag(),
                            );
                            ui.ctx().accesskit_node_builder(r.id, |n| {
                                n.set_role(egui::accesskit::Role::Tab);
                                n.set_label(ax_name.as_str());
                                n.set_selected(is_active);
                                n.add_action(egui::accesskit::Action::Click);
                            });
                            tab_stops.push((r.id, close_id));
                            if is_active {
                                active_stop = Some(r.id);
                            }
                            if r.clicked() {
                                let on_close = close_rect.is_some_and(|c| {
                                    r.interact_pointer_pos().is_some_and(|p| c.contains(p))
                                });
                                if on_close {
                                    close = Some(tab.id);
                                } else {
                                    activate = Some(i);
                                }
                            }
                            if r.middle_clicked() {
                                close = Some(tab.id);
                            }
                            if r.dragged() {
                                dragging = Some(i);
                            }
                            if r.drag_stopped() {
                                dropped = Some(i);
                            }
                            r.context_menu(|ui| {
                                let (label, on) = if tab.pinned {
                                    (&unpin_l, false)
                                } else {
                                    (&pin_l, true)
                                };
                                if ui.button(label).clicked() {
                                    pin = Some((i, on));
                                    ui.close();
                                }
                                if ui.button(&close_l).clicked() {
                                    close = Some(tab.id);
                                    ui.close();
                                }
                                if ui.button(&close_others_l).clicked() {
                                    tab_cmd = Some((tab.id, MenuAction::CloseOthers));
                                    ui.close();
                                }
                                if ui.button(&close_all_l).clicked() {
                                    tab_cmd = Some((tab.id, MenuAction::CloseAll));
                                    ui.close();
                                }
                                if tab.editor.file().is_some() {
                                    ui.separator();
                                    if ui.button(&copy_path_l).clicked() {
                                        tab_cmd = Some((tab.id, MenuAction::CopyPath));
                                        ui.close();
                                    }
                                    if ui.button(&reveal_l).clicked() {
                                        tab_cmd = Some((tab.id, MenuAction::Reveal));
                                        ui.close();
                                    }
                                }
                            });
                        }
                    });
                });
            let h = scroll.content_size.y.max(ui.spacing().interact_size.y);
            let (rect, r) = ui.allocate_exact_size(egui::vec2(NEW_TAB_W, h), egui::Sense::click());
            let r = r.on_hover_ui(|ui| {
                ui.set_max_width(ui.spacing().tooltip_width);
                let tip = match &new_tab_key {
                    Some(s) => format!("{new_tab_l} ({})", ui.ctx().format_shortcut(s)),
                    None => new_tab_l.clone(),
                };
                ui.label(tip);
            });
            ui.ctx().accesskit_node_builder(r.id, |n| {
                n.set_role(egui::accesskit::Role::Button);
                n.set_label(new_tab_l.as_str());
                if let Some(d) = &new_tab_desc {
                    n.set_description(d.as_str());
                }
            });
            if ui.is_rect_visible(rect) {
                let painter = ui.painter();
                if r.is_pointer_button_down_on() {
                    painter.rect_filled(rect, 0.0, p.border);
                } else if r.hovered() {
                    painter.rect_filled(rect, 0.0, p.canvas);
                }
                painter.hline(
                    rect.x_range(),
                    rect.bottom() - 0.5,
                    egui::Stroke::new(1.0, p.border),
                );
                let color = if r.hovered() || r.has_focus() {
                    p.text
                } else {
                    p.muted
                };
                let stroke = egui::Stroke::new(1.5, color);
                let c = rect.center();
                let arm = 5.0;
                painter.line_segment([c - egui::vec2(arm, 0.0), c + egui::vec2(arm, 0.0)], stroke);
                painter.line_segment([c - egui::vec2(0.0, arm), c + egui::vec2(0.0, arm)], stroke);
                if r.has_focus() {
                    let ring = ui.visuals().selection.stroke;
                    painter.rect_stroke(
                        rect.shrink(2.0),
                        2.0,
                        egui::Stroke::new(ring.width.max(1.5), ring.color),
                        egui::StrokeKind::Inside,
                    );
                }
            }
            if r.clicked() {
                new_tab = true;
            }
            r.id
        });
        let new_tab_id = outer.inner;
        // Double-clicking the empty strip after the tabs opens a new tab.
        let rest = egui::Rect::from_min_max(
            egui::pos2(outer.response.rect.right(), outer.response.rect.top()),
            egui::pos2(ui.max_rect().right(), outer.response.rect.bottom()),
        );
        if rest.width() > 0.0 {
            // Pointer only: not a keyboard stop of its own.
            let id = egui::Id::unique("text3-tab-strip-rest");
            let r = ui.interact(rest, id, egui::Sense::CLICK);
            ui.ctx().accesskit_node_builder(id, |n| n.set_hidden());
            if r.double_clicked() {
                new_tab = true;
            }
        }
        self.tab_in_view = in_view;
        self.focus.now.tabs = crate::focus::tab_strip_stops(tab_stops, Some(new_tab_id));
        self.focus.now.active_tab = active_stop;
        // Drag to reorder: the drop position is the gap nearest the pointer.
        let target = |x: f32| rects.iter().take_while(|r| r.center().x < x).count();
        if let (Some(from), Some(pos)) = (dragging, ctx.pointer_latest_pos()) {
            let gap = target(pos.x);
            if gap != from && gap != from + 1 {
                let x = rects
                    .get(gap)
                    .map_or_else(|| rects.last().map_or(0.0, |r| r.right()), |r| r.left());
                let y = rects
                    .first()
                    .map_or(egui::Rangef::new(0.0, 0.0), |r| r.y_range());
                ui.painter().vline(x, y, egui::Stroke::new(2.0, p.accent));
            }
            ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
        }
        if let (Some(from), Some(pos)) = (dropped, ctx.pointer_latest_pos()) {
            let gap = target(pos.x);
            let to = if gap > from { gap - 1 } else { gap };
            self.ws.move_tab(from, to);
        }
        if new_tab {
            self.dispatch(MenuAction::New, ctx);
        }
        if let Some((i, on)) = pin {
            self.ws.set_pinned(i, on);
        }
        match tab_cmd {
            Some((id, MenuAction::CloseOthers)) => self.close_many(Some(id), ctx),
            Some((_, MenuAction::CloseAll)) => self.close_many(None, ctx),
            Some((id, MenuAction::CopyPath)) => self.copy_path(id),
            Some((id, MenuAction::Reveal)) => self.reveal(id),
            _ => {}
        }
        if let Some(i) = activate {
            self.ws.activate(i);
            self.focus_active();
        }
        if let Some(id) = close {
            self.request_close_tab(id, ctx);
        }
    }

    /// The count item's text and its full spoken form.
    fn count_label(&self, v: &text3_app::counts::View) -> (String, String) {
        let num = |n: Option<u64>| n.map_or_else(|| "—".to_owned(), |n| self.l10n.number(n));
        let doc = v.doc.map(|s| s.counts);
        let sel = v.sel.map(|s| s.counts);
        let label = if v.selected {
            self.t(
                "count-status-selection",
                &[
                    ("chars", num(sel.map(|c| c.chars)).into()),
                    ("total-chars", num(doc.map(|c| c.chars)).into()),
                    ("words", num(sel.map(|c| c.words)).into()),
                    ("total-words", num(doc.map(|c| c.words)).into()),
                ],
            )
        } else {
            self.t(
                "count-status",
                &[
                    ("chars", num(doc.map(|c| c.chars)).into()),
                    ("words", num(doc.map(|c| c.words)).into()),
                ],
            )
        };
        let missing = doc.is_none() || (v.selected && sel.is_none());
        let spoken = if missing {
            self.t("count-a11y-pending", &[])
        } else {
            label.clone()
        };
        (label, spoken)
    }

    /// The counts summary: characters with and without spaces, words,
    /// lines and bytes, for the selection and the document.
    fn count_summary(&self, ui: &mut egui::Ui, v: &text3_app::counts::View, enc: TextEncoding) {
        use text3_app::counts::Stats;
        let num = |n: Option<u64>| n.map_or_else(|| "—".to_owned(), |n| self.l10n.number(n));
        let mut columns: Vec<(Option<String>, Option<Stats>)> = Vec::new();
        if v.selected {
            columns.push((Some(self.t("count-heading-selection", &[])), v.sel));
            columns.push((Some(self.t("count-heading-document", &[])), v.doc));
        } else {
            columns.push((None, v.doc));
        }
        type Field = fn(&Stats) -> Option<u64>;
        let rows: [(String, Field); 5] = [
            (self.t("count-characters", &[]), |s| Some(s.counts.chars)),
            (self.t("count-characters-no-spaces", &[]), |s| {
                Some(s.counts.non_space)
            }),
            (self.t("count-words", &[]), |s| Some(s.counts.words)),
            (self.t("count-lines", &[]), |s| Some(s.counts.lines)),
            (
                self.t("count-bytes", &[("encoding", enc.name().into())]),
                |s| s.bytes,
            ),
        ];
        ui.set_min_width(240.0);
        egui::Grid::new("count-summary")
            .num_columns(columns.len() + 1)
            .spacing([16.0, 4.0])
            .striped(true)
            .show(ui, |ui| {
                if v.selected {
                    ui.label("");
                    for (head, _) in &columns {
                        ui.strong(head.clone().unwrap_or_default());
                    }
                    ui.end_row();
                }
                for (name, get) in &rows {
                    ui.label(name);
                    for (_, stats) in &columns {
                        ui.label(num(stats.as_ref().and_then(get)));
                    }
                    ui.end_row();
                }
            });
        let unencodable = columns
            .iter()
            .any(|(_, s)| s.is_some_and(|s| s.bytes.is_none()));
        if unencodable {
            ui.weak(self.t("count-unencodable", &[("encoding", enc.name().into())]));
        }
        if v.pending {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(self.t("count-counting", &[]));
            });
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let p = theme::palette(ui.ctx());
        let id = self.ws.active().id;
        let mut choice: Option<Option<Option<Lang>>> = None;
        let mut action: Option<MenuAction> = None;
        let mut indent_settings = false;
        let auto_l = self.t("language-auto", &[]);
        let plain_l = self.t("language-plain", &[]);
        let tip = self.t("language-choose", &[]);
        let current_choice = self.lang_override.get(&id).copied();
        let can_reopen = self.enabled(MenuAction::ReopenWithEncoding);
        // Character and word count; not for the log viewer.
        let count_view = (self.settings.show_counts && self.log.is_none()).then(|| {
            let tab = self.ws.active();
            let ed = &tab.editor;
            let doc = ed.document();
            self.counts.view(
                tab.id,
                doc.rope(),
                doc.revision(),
                ed.encoding(),
                ed.selections().iter().map(|s| s.range()),
                self.counts_open,
            )
        });
        if count_view.as_ref().is_some_and(|v| v.pending) {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(16));
        }
        let mut counts_open = false;
        let editor = self.editor();
        // Keyboard stops, created right to left (absent ones `None`).
        let mut stops: Vec<Option<egui::Id>> = Vec::new();
        ui.horizontal(|ui| {
            if editor.is_recording() {
                ui.colored_label(p.error, "●");
                ui.colored_label(p.text, self.t("macro-recording", &[]));
            }
            if let Some((msg, is_error)) = &self.status {
                let r = ui.colored_label(if *is_error { p.error } else { p.muted }, msg);
                // Spoken when it appears or changes (saved, refused, ...).
                crate::structured::announce(ui, r.id);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let muted = |t: String| egui::RichText::new(t).color(p.muted);
                // Encoding: reopen or save with another one.
                let r = ui
                    .menu_button(muted(editor.encoding().name().to_owned()), |ui| {
                        for (a, enabled) in [
                            (MenuAction::ReopenWithEncoding, can_reopen),
                            (MenuAction::SaveWithEncoding, true),
                        ] {
                            if ui
                                .add_enabled(enabled, egui::Button::new(self.t(a.label_id(), &[])))
                                .clicked()
                            {
                                action = Some(a);
                                ui.close();
                            }
                        }
                    })
                    .response
                    .on_hover_text(self.t("status-encoding-choose", &[]));
                stops.push(Some(r.id));
                let mut indent_stop = None;
                if let Some((_, ind)) = self.tab_indent.get(&self.ws.active().id) {
                    let label = match ind.text(
                        !self.settings.insert_spaces,
                        usize::from(self.settings.tab_width),
                    ) {
                        t if t == "\t" => self.t("status-indent-tabs", &[]),
                        t => self.t(
                            "status-indent-spaces",
                            &[("count", (t.len() as i64).into())],
                        ),
                    };
                    // Indentation: the Editor settings. Framed like the menu
                    // buttons beside it.
                    let r = ui
                        .add(egui::Button::new(muted(label)))
                        .on_hover_text(self.t("status-indent-choose", &[]));
                    if r.clicked() {
                        indent_settings = true;
                    }
                    indent_stop = Some(r.id);
                }
                stops.push(indent_stop);
                let current_le = editor.line_ending();
                let le = match current_le {
                    LineEnding::CrLf => "line-ending-crlf",
                    LineEnding::Cr => "line-ending-cr",
                    LineEnding::Mixed => "line-ending-mixed",
                    LineEnding::Lf | LineEnding::None => "line-ending-lf",
                };
                // Line endings: convert the document.
                let r = ui
                    .menu_button(muted(self.t(le, &[])), |ui| {
                        for (a, label, on) in [
                            (
                                MenuAction::ToLf,
                                "line-ending-lf",
                                matches!(current_le, LineEnding::Lf | LineEnding::None),
                            ),
                            (
                                MenuAction::ToCrLf,
                                "line-ending-crlf",
                                current_le == LineEnding::CrLf,
                            ),
                        ] {
                            if ui.selectable_label(on, self.t(label, &[])).clicked() {
                                action = Some(a);
                                ui.close();
                            }
                        }
                    })
                    .response
                    .on_hover_text(self.t("status-line-ending-choose", &[]));
                stops.push(Some(r.id));
                let lang = match self.language(id, editor) {
                    Some(l) => l.name().to_owned(),
                    None => plain_l.clone(),
                };
                // Click the language to choose another one for this tab.
                let r = ui
                    .menu_button(muted(lang), |ui| {
                        if ui
                            .selectable_label(current_choice.is_none(), &auto_l)
                            .clicked()
                        {
                            choice = Some(None);
                            ui.close();
                        }
                        if ui
                            .selectable_label(current_choice == Some(None), &plain_l)
                            .clicked()
                        {
                            choice = Some(Some(None));
                            ui.close();
                        }
                        ui.separator();
                        egui::ScrollArea::vertical()
                            .max_height(320.0)
                            .show(ui, |ui| {
                                for l in Lang::ALL {
                                    if ui
                                        .selectable_label(current_choice == Some(Some(l)), l.name())
                                        .clicked()
                                    {
                                        choice = Some(Some(Some(l)));
                                        ui.close();
                                    }
                                }
                            });
                    })
                    .response
                    .on_hover_text(&tip);
                stops.push(Some(r.id));
                // Characters and words: click for the full summary.
                let mut count_stop = None;
                if let Some(v) = &count_view {
                    let (label, spoken) = self.count_label(v);
                    let r = ui
                        .menu_button(muted(label), |ui| {
                            counts_open = true;
                            self.count_summary(ui, v, editor.encoding());
                        })
                        .response
                        .on_hover_text(self.t("count-choose", &[]));
                    ui.ctx()
                        .accesskit_node_builder(r.id, |n| n.set_label(spoken));
                    count_stop = Some(r.id);
                }
                stops.push(count_stop);
                let sels = editor.selections();
                if sels.len() > 1 {
                    let n = sels.len() as i64;
                    ui.colored_label(p.muted, self.t("status-selections", &[("count", n.into())]));
                }
                let (line, col) =
                    text3_core::selection::line_col(editor.document().rope(), sels.primary().head);
                let pos = self.t(
                    "status-position",
                    &[
                        ("line", (line as i64 + 1).into()),
                        ("column", (col as i64 + 1).into()),
                    ],
                );
                ui.colored_label(p.muted, pos);
            });
        });
        self.focus.now.status = crate::focus::status_bar_stops(stops);
        self.counts_open = counts_open;
        if indent_settings {
            self.settings_tab = SettingsTab::Editor;
            self.settings_open = true;
            self.settings_focus = true;
        }
        if let Some(a) = action {
            let ctx = ui.ctx().clone();
            self.dispatch(a, &ctx);
        }
        match choice {
            Some(None) => {
                self.lang_override.remove(&id);
            }
            Some(Some(l)) => {
                self.lang_override.insert(id, l);
            }
            None => return,
        }
        // Highlight again with the new language.
        self.requested.remove(&id);
        self.spans.remove(&id);
        if let Some(t) = self.ws.position(id).and_then(|i| self.ws.tab_mut(i)) {
            t.editor.clear_syntax_folds();
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.dialog.clone() else {
            return;
        };
        let name = self.display_name();
        let mut close = false;
        let mut after: Option<Action> = None;
        egui::Modal::new(egui::Id::unique("text3-dialog")).show(ctx, |ui| {
            ui.set_max_width(420.0);
            let (title, body) = match &dialog {
                Dialog::Unsaved(_) => (
                    self.t("dialog-unsaved-title", &[("name", name.clone().into())]),
                    self.t("dialog-unsaved-body", &[]),
                ),
                Dialog::Conflict => (
                    self.t("dialog-conflict-title", &[("name", name.clone().into())]),
                    self.t("dialog-conflict-body", &[]),
                ),
                Dialog::ReadOnly => (
                    self.t("dialog-readonly-title", &[("name", name.clone().into())]),
                    self.t("dialog-readonly-body", &[]),
                ),
                Dialog::Error(msg) => (msg.clone(), String::new()),
                Dialog::OpenLink(url) => (self.t("dialog-open-link-title", &[]), url.clone()),
                Dialog::ConfirmRevert => (
                    self.t("revert-title", &[("name", self.display_name().into())]),
                    self.t("revert-body", &[]),
                ),
                Dialog::ConfirmPurge => (
                    self.t("history-purge-title", &[]),
                    self.t("history-purge-body", &[]),
                ),
                Dialog::ConfirmReplaceFiles(count, files) => (
                    self.t(
                        "replace-files-title",
                        &[
                            ("count", (*count as i64).into()),
                            ("files", (*files as i64).into()),
                        ],
                    ),
                    self.t("replace-files-body", &[]),
                ),
                Dialog::Update(version, _) => (
                    self.t(
                        "update-available-title",
                        &[("version", version.clone().into())],
                    ),
                    self.t("update-available-body", &[]),
                ),
            };
            ui.strong(title);
            if !body.is_empty() {
                ui.label(body);
            }
            ui.add_space(8.0);
            ui.with_layout(
                egui::Layout::right_to_left(egui::Align::Center),
                |ui| match &dialog {
                    Dialog::Unsaved(then) => {
                        if ui.button(self.t("button-save", &[])).clicked() {
                            close = true;
                            after = Some(Action::SaveThen(*then));
                        }
                        if ui.button(self.t("button-dont-save", &[])).clicked() {
                            close = true;
                            after = Some(Action::Proceed(*then));
                        }
                        if ui.button(self.t("button-cancel", &[])).clicked() {
                            close = true;
                        }
                    }
                    Dialog::Conflict => {
                        if ui.button(self.t("button-save-as", &[])).clicked() {
                            close = true;
                            after = Some(Action::SaveAs);
                        }
                        if ui.button(self.t("button-overwrite", &[])).clicked() {
                            close = true;
                            after = Some(Action::Overwrite);
                        }
                        if ui.button(self.t("button-cancel", &[])).clicked() {
                            close = true;
                        }
                    }
                    Dialog::ReadOnly => {
                        if ui.button(self.t("button-save-as", &[])).clicked() {
                            close = true;
                            after = Some(Action::SaveAs);
                        }
                        if ui.button(self.t("button-cancel", &[])).clicked() {
                            close = true;
                        }
                    }
                    Dialog::ConfirmRevert => {
                        if ui.button(self.t("revert-confirm", &[])).clicked() {
                            close = true;
                            after = Some(Action::Revert);
                        }
                        if ui.button(self.t("button-cancel", &[])).clicked() {
                            close = true;
                        }
                    }
                    Dialog::ConfirmReplaceFiles(..) => {
                        if ui.button(self.t("replace-files-confirm", &[])).clicked() {
                            close = true;
                            after = Some(Action::ReplaceInFiles);
                        }
                        if ui.button(self.t("button-cancel", &[])).clicked() {
                            close = true;
                        }
                    }
                    Dialog::ConfirmPurge => {
                        if ui.button(self.t("history-purge-confirm", &[])).clicked() {
                            close = true;
                            after = Some(Action::PurgeHistory);
                        }
                        if ui.button(self.t("button-cancel", &[])).clicked() {
                            close = true;
                        }
                    }
                    Dialog::Update(_, page) => {
                        if ui.button(self.t("update-open-page", &[])).clicked() {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(page));
                            close = true;
                        }
                        if ui.button(self.t("update-later", &[])).clicked() {
                            close = true;
                        }
                    }
                    Dialog::OpenLink(url) => {
                        if ui.button(self.t("button-open", &[])).clicked() {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                            close = true;
                        }
                        if ui.button(self.t("button-cancel", &[])).clicked() {
                            close = true;
                        }
                    }
                    Dialog::Error(_) => {
                        if ui.button(self.t("button-ok", &[])).clicked() {
                            close = true;
                        }
                    }
                },
            );
        });
        if close {
            self.dialog = None;
            if after.is_none() {
                // Cancelled: a later quit records the session afresh (the
                // texts hot exit wrote stay as ordinary checkpoints).
                self.quit_recorded = false;
                self.hot_exited.clear();
            }
            self.focus_active();
            match after {
                Some(Action::SaveThen(then)) => {
                    if self.save() {
                        self.proceed(then, ctx);
                    }
                }
                Some(Action::Proceed(then)) => self.proceed(then, ctx),
                Some(Action::SaveAs) => {
                    self.save_as();
                }
                Some(Action::Revert) => self.revert(),
                Some(Action::ReplaceInFiles) => self.replace_in_files(ctx),
                Some(Action::PurgeHistory) => {
                    let msg = match self
                        .history
                        .as_ref()
                        .map(text3_io::history::HistoryStore::purge_all)
                    {
                        Some(Ok(())) => (self.t("history-purged", &[]), false),
                        Some(Err(e)) => (e.to_string(), true),
                        None => return,
                    };
                    self.status = Some(msg);
                    self.history_open = false;
                }
                Some(Action::Overwrite) => {
                    let o = self.editor_mut().save_overwrite();
                    self.handle_save(o);
                }
                None => {}
            }
        }
    }
}

impl eframe::App for Shell {
    #[cfg(not(windows))]
    fn on_exit(&mut self) {
        self.exiting();
    }

    // Windows builds include the glow renderer, which changes the signature.
    #[cfg(windows)]
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.exiting();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.ctx = ctx.clone();
        // TEXT3_MEASURE_EXIT=1: the start-up has been measured.
        if text3_app::startprobe::frame_start() {
            std::process::exit(0);
        }
        // TEXT3_BENCH: the next step of the measured workload.
        if self.bench.is_some() {
            self.bench_frame_start(&ctx);
        }
        // The first frame has been presented when the second one starts.
        #[cfg(target_os = "macos")]
        if self.menu_pending && ctx.cumulative_frame_nr() > 0 {
            self.build_pending_menu(&ctx);
        }
        self.poll_fonts(&ctx);
        self.focus_frame_start(&ctx);
        // Files dropped onto the window open in tabs.
        // Files opened from the system (Finder on macOS) are handled the same way.
        let mut dropped: Vec<std::path::PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        dropped.extend(text3_platform::take_opened_files());
        for p in dropped {
            if p.is_file() {
                self.open(&p);
            }
        }

        // A quit attempt that neither closed the window nor left a dialog
        // open (a cancelled Save As, a save that hit a conflict, …) is
        // over: the next attempt starts afresh.
        if self.quit_recorded && !self.allow_close && self.dialog.is_none() {
            self.quit_recorded = false;
            self.hot_exited.clear();
        }
        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            if self.dialog.is_none() {
                self.request_quit(&ctx);
            }
        }

        let mut actions = Vec::new();
        // Cmd/Ctrl+= also zooms in (US layouts), whatever the menu shows.
        if self.dialog.is_none() {
            let eq = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Equals);
            let plus = egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Plus);
            if ctx.input_mut(|i| i.consume_shortcut(&eq) || i.consume_shortcut(&plus)) {
                actions.push(MenuAction::ZoomIn);
            }
        }
        #[cfg(target_os = "macos")]
        if let Some(m) = &self.native_menu {
            actions.extend(m.poll());
        }
        // Without a native menu, file, tab and quit shortcuts are handled
        // here; editing shortcuts arrive as key events in the view.
        if !self.has_native_menu() && self.dialog.is_none() && self.recording.is_none() {
            let text_field = self.text_field_focused(&ctx);
            for a in MenuAction::ALL {
                // Clipboard shortcuts arrive as clipboard events; editing
                // commands belong to the focused text field if it is not
                // the editor.
                let skip = matches!(a, MenuAction::Cut | MenuAction::Copy | MenuAction::Paste)
                    || (text_field && a.edits_text());
                if skip {
                    continue;
                }
                if let Some(s) = a.shortcut(&self.settings.keymap)
                    && ctx.input_mut(|i| {
                        // Arrow, Tab and function key shortcuts must match
                        // all modifiers: egui's logical matching ignores
                        // extra Shift/Alt and would steal rectangular
                        // selection or duplicate-line keys, and run F6 for
                        // Shift+F6 (Ctrl+Tab for Ctrl+Shift+Tab).
                        use egui::Key as K;
                        if matches!(
                            s.logical_key,
                            K::ArrowUp
                                | K::ArrowDown
                                | K::ArrowLeft
                                | K::ArrowRight
                                | K::Tab
                                | K::F1
                                | K::F2
                                | K::F3
                                | K::F4
                                | K::F5
                                | K::F6
                                | K::F7
                                | K::F8
                                | K::F9
                                | K::F10
                                | K::F11
                                | K::F12
                        ) {
                            let mut consumed = false;
                            i.events.retain(|event| {
                                let matches = matches!(event, egui::Event::Key {
                                    key, pressed: true, modifiers, ..
                                } if *key == s.logical_key && modifiers.matches_exact(s.modifiers));
                                consumed |= matches;
                                !matches
                            });
                            consumed
                        } else {
                            i.consume_shortcut(&s)
                        }
                    })
                {
                    actions.push(a);
                }
            }
        }

        if !self.has_native_menu() {
            let p = theme::palette(&ctx);
            let frame = egui::Frame::NONE
                .fill(p.surface)
                .inner_margin(egui::Margin::symmetric(6, 2));
            egui::Panel::top("menu").frame(frame).show(ui, |ui| {
                let label = |id: &str| self.t(id, &[]);
                let enabled = |a| self.enabled(a);
                if let Some(a) = menu::in_window(ui, &label, enabled, &self.settings.keymap) {
                    actions.push(a);
                }
            });
        }
        for a in actions {
            self.dispatch(a, &ctx);
        }
        #[cfg(target_os = "macos")]
        if let Some(m) = &self.native_menu {
            m.set_enabled(|a| self.enabled(a));
        }
        if let Some((region, entry)) = self.focus.pending.take() {
            self.focus.enter(&ctx, region, entry);
        }
        if let Some(from) = self.focus.back_from.take() {
            ctx.memory_mut(|m| {
                m.request_focus(from);
                m.move_focus(egui::FocusDirection::Previous);
            });
        }

        let p = theme::palette(&ctx);
        egui::Panel::top("tabs")
            .frame(egui::Frame::NONE.fill(p.surface))
            .show(ui, |ui| self.tab_bar(ui, &ctx));
        if self.find.open {
            let tab = self.ws.active().id;
            self.find.refresh(tab, &self.ws.active().editor, &ctx);
            let current = self
                .find
                .matches
                .index_of(&self.editor().selections().primary().range());
            let l10n = &self.l10n;
            let t = |id: &str, a: Option<(&str, i64)>, b: Option<(&str, i64)>| {
                let args: Vec<(&str, FluentValue<'_>)> = [a, b]
                    .into_iter()
                    .flatten()
                    .map(|(k, v)| (k, v.into()))
                    .collect();
                tr(l10n, id, &args)
            };
            let frame = egui::Frame::NONE
                .fill(p.surface)
                .inner_margin(egui::Margin::symmetric(8, 6));
            let mut event = None;
            let focus = &mut self.focus;
            let r = egui::Panel::top("find").frame(frame).show(ui, |ui| {
                focus.edge(ui, Region::Find, Edge::Head);
                event = self.find.show(ui, &t, current);
                focus.edge(ui, Region::Find, Edge::Tail);
            });
            self.focus.now.layout.find = true;
            self.focus.now.rects.push((Region::Find, r.response.rect));
            if let Some(e) = event {
                self.find_event(e);
            }
        }
        self.update_syntax(&ctx);
        let r = egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        self.focus.now.rects.push((Region::Status, r.response.rect));
        if self.folder.open {
            let l10n = &self.l10n;
            let t = |id: &str, a: Option<(&str, i64)>, b: Option<(&str, i64)>| {
                let args: Vec<(&str, FluentValue<'_>)> = [a, b]
                    .into_iter()
                    .flatten()
                    .map(|(k, v)| (k, v.into()))
                    .collect();
                tr(l10n, id, &args)
            };
            let mut event = None;
            let focus = &mut self.focus;
            let r = egui::Panel::bottom("folder-search")
                .resizable(true)
                .default_size(240.0)
                .frame(
                    egui::Frame::NONE
                        .fill(p.surface)
                        .inner_margin(egui::Margin::same(8)),
                )
                .show(ui, |ui| {
                    focus.edge(ui, Region::Folder, Edge::Head);
                    event = self.folder.ui(ui, &t);
                    focus.edge(ui, Region::Folder, Edge::Tail);
                });
            self.focus.now.layout.folder = true;
            self.focus.now.rects.push((Region::Folder, r.response.rect));
            use crate::folderpanel::PanelEvent;
            match event {
                Some(PanelEvent::Close) => {
                    self.folder.open = false;
                    self.focus_active();
                }
                Some(PanelEvent::ChooseFolder) => {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        self.folder.root = Some(dir);
                    }
                }
                Some(PanelEvent::ReplaceInFiles) => {
                    let (count, files) = (self.folder.hit_count(), self.folder.hit_files().len());
                    self.dialog = Some(Dialog::ConfirmReplaceFiles(count, files));
                }
                Some(PanelEvent::OpenHit(hit)) => {
                    self.open(&hit.path);
                    if self.editor().file().is_some_and(|f| f.path == hit.path) {
                        self.editor_mut().select(hit.chars.clone());
                        let id = self.ws.active().id;
                        self.active_view(id).reveal();
                        self.focus_active();
                    }
                }
                None => {}
            }
        }
        self.outline_panel(ui);
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(p.canvas))
            .show(ui, |ui| {
                let active = self.ws.active().id;
                let comparing = self.compare.as_ref().is_some_and(|c| c.left == active);
                if self.log.is_none() && !comparing && self.split_area(ui) {
                    return;
                }
                // One area: the log, a comparison or the active tab.
                let rect = ui.max_rect();
                let pane = Region::Pane(0);
                self.focus.edge(ui, pane, Edge::Head);
                let entry = if self.log_area(ui) || self.compare_area(ui) {
                    None
                } else {
                    let id = self.ws.active().id;
                    self.pane_area(ui, id, 0);
                    self.pane_entry(id, 0)
                };
                self.focus.edge(ui, pane, Edge::Tail);
                self.focus.now.panes.push(crate::focus::PaneStop {
                    slot: 0,
                    entry,
                    rect,
                });
            });
        self.take_focus_go();

        self.poll_update();
        self.dialogs(&ctx);
        self.settings_window(&ctx);
        self.default_apps_window(&ctx);
        self.compare_picker(&ctx);
        self.history_window(&ctx);
        self.replace_preview_window(&ctx);
        self.about_window(&ctx);
        self.encoding_window(&ctx);
        // After a command run from the palette: keyboard focus goes back
        // to the document unless the command moved it somewhere (a dialog,
        // another prompt, a window's field).
        if std::mem::take(&mut self.refocus)
            && !self.palette.open
            && self.dialog.is_none()
            && ctx
                .memory(|m| m.focused())
                .is_none_or(|f| Some(f) == self.palette.field)
        {
            self.focus_active();
        }
        if !self.palette.open || self.palette.mode != crate::palette::Mode::Anything {
            self.end_goto();
        }
        if self.palette.open
            && self.dialog.is_none()
            && self.palette.mode != crate::palette::Mode::Commands
        {
            self.goto_palette(&ctx);
        } else if self.palette.open && self.dialog.is_none() {
            let entries: Vec<Entry> = MenuAction::ALL
                .into_iter()
                .filter(|a| *a != MenuAction::CommandPalette)
                .filter(|a| !(text3_app::STORE && *a == MenuAction::CheckUpdates))
                .map(|a| Entry {
                    action: a,
                    label: self.t(a.label_id(), &[]).trim_end_matches('…').to_owned(),
                    shortcut: a
                        .shortcut(&self.settings.keymap)
                        .map(|s| ctx.format_shortcut(&s))
                        .unwrap_or_default(),
                    enabled: self.enabled(a),
                })
                .collect();
            let hint = self.t("palette-placeholder", &[]);
            if let Some(a) = self.palette.show(&ctx, &entries, &hint) {
                self.dispatch(a, &ctx);
                self.refocus = true;
                ctx.request_repaint();
            } else if !self.palette.open {
                self.focus_active();
            }
        }
        self.apply_theme(&ctx);
        self.focus.end_frame(&ctx);

        let (wait, error) = self.ws.tick(std::time::Instant::now());
        if let Some(wait) = wait {
            ctx.request_repaint_after(wait);
        }
        if let Some(reason) = error {
            let msg = self.t("status-recovery-failed", &[("reason", reason.into())]);
            self.status = Some((msg, true));
        }

        let dirty = if self.editor().document().is_dirty() {
            "yes"
        } else {
            "no"
        };
        let title = self.t(
            "window-title",
            &[
                ("name", self.display_name().into()),
                ("dirty", dirty.into()),
            ],
        );
        if title != self.title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.title = title;
        }
        if text3_app::startprobe::active() {
            let text_shown = self.log.is_some() || self.editor().document().len_chars() > 0;
            if text3_app::startprobe::frame_end(text_shown) {
                ctx.request_repaint();
            }
        }
        if self.bench.is_some() {
            self.bench_frame_end(&ctx);
        }
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if self.bench.is_some() {
            self.bench_input(raw_input);
        }
    }
}

/// Localized reasons why part of a YAML tree is read-only.
fn yaml_lock_labels(l10n: &Localizer) -> Vec<(text3_tools::yaml::Lock, String)> {
    use text3_tools::yaml::Lock;
    [
        (Lock::Flow, "yaml-read-only-flow"),
        (Lock::Anchor, "yaml-read-only-anchor"),
        (Lock::Tag, "yaml-read-only-tag"),
        (Lock::Alias, "yaml-read-only-alias"),
        (Lock::BlockScalar, "yaml-read-only-block-scalar"),
        (Lock::ComplexKey, "yaml-read-only-complex-key"),
        (Lock::OtherDocument, "yaml-read-only-document"),
        (Lock::Layout, "yaml-read-only-layout"),
        (Lock::OnlyChild, "yaml-read-only-only-child"),
    ]
    .into_iter()
    .map(|(l, id)| (l, tr(l10n, id, &[])))
    .collect()
}

/// Localization ID for a JSON error reason (see locales: `json-reason-*`).
fn json_reason_id(message: &str) -> String {
    let mut id = String::from("json-reason-");
    let mut dash = false;
    for c in message.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            if dash && !id.ends_with('-') {
                id.push('-');
            }
            id.push(c);
            dash = false;
        } else {
            dash = true;
        }
    }
    id
}

/// Format a localized message. Fluent's isolation marks are stripped
/// because egui renders them as visible boxes.
/// A folder for display in a narrow column: the home folder as `~` and
/// at most 40 characters, keeping the end (`…/project/src`).
fn short_dir(dir: &Path) -> String {
    let mut s = dir.to_string_lossy().into_owned();
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
        && let Some(rest) = s.strip_prefix(&*home.to_string_lossy())
    {
        s = format!("~{rest}");
    }
    let n = s.chars().count();
    if n > 40 {
        let tail: String = s.chars().skip(n - 39).collect();
        s = format!("…{tail}");
    }
    s
}

/// Import a Notepad++ `shortcuts.xml` into `keymap`; returns the localized
/// summary and whether it is an error (the keymap is unchanged then).
fn import_npp_shortcuts(
    path: &Path,
    keymap: &mut text3_app::keymap::Keymap,
    l10n: &Localizer,
) -> (String, bool) {
    let lt = |id: &str, args: &[(&str, FluentValue<'_>)]| tr(l10n, id, args);
    use std::io::Read;
    use text3_app::npp_shortcuts::{self, ImportError, MAX_BYTES};
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let fail = |reason: String| {
        (
            lt(
                "settings-shortcut-import-npp-failed",
                &[("name", name.clone().into()), ("reason", reason.into())],
            ),
            true,
        )
    };
    let mut bytes = Vec::new();
    // Read at most one byte past the cap so a huge file is not loaded.
    if let Err(e) =
        std::fs::File::open(path).and_then(|f| f.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes))
    {
        return fail(e.to_string());
    }
    match npp_shortcuts::parse(&bytes) {
        Ok(import) => {
            let conflicts = menu::apply_import(keymap, &import.entries);
            (
                lt(
                    "settings-shortcut-import-npp-done",
                    &[
                        ("imported", import.entries.len().into()),
                        ("skipped", import.skipped.into()),
                        ("conflicts", conflicts.into()),
                    ],
                ),
                false,
            )
        }
        Err(ImportError::TooLarge) => fail(lt("settings-shortcut-import-npp-too-large", &[])),
        Err(ImportError::Invalid) => fail(lt("settings-shortcut-import-npp-invalid", &[])),
    }
}

fn tr(l10n: &Localizer, id: &str, args: &[(&str, FluentValue<'_>)]) -> String {
    let mut fa = text3_i18n::FluentArgs::new();
    for (k, v) in args {
        fa.set(*k, v.clone());
    }
    l10n.format(id, Some(&fa))
        .replace(['\u{2068}', '\u{2069}'], "")
}

pub fn describe_load_error(l10n: &Localizer, path: &Path, e: &LoadError) -> String {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let mut args = text3_i18n::FluentArgs::new();
    args.set("name", name);
    let id = match e {
        LoadError::Decode { .. } => "error-open-encoding",
        LoadError::Io { .. } => "error-open",
    };
    l10n.format(id, Some(&args))
        .replace(['\u{2068}', '\u{2069}'], "")
}

#[cfg(test)]
mod tests {
    use super::{MdMode, PaneContent, pane_content};

    #[test]
    fn pane_shows_the_markdown_mode_of_its_tab() {
        assert_eq!(
            pane_content(false, Some(MdMode::Source)),
            PaneContent::Editor
        );
        assert_eq!(
            pane_content(false, Some(MdMode::Preview)),
            PaneContent::Preview
        );
        assert_eq!(
            pane_content(false, Some(MdMode::Split)),
            PaneContent::EditorAndPreview
        );
    }

    #[test]
    fn pane_shows_structured_view_when_on() {
        assert_eq!(pane_content(true, None), PaneContent::Structured);
        // The structured view wins, as in the single area.
        assert_eq!(
            pane_content(true, Some(MdMode::Preview)),
            PaneContent::Structured
        );
    }

    #[test]
    fn other_tabs_show_the_plain_editor() {
        assert_eq!(pane_content(false, None), PaneContent::Editor);
    }

    #[test]
    fn json_reason_ids_exist_in_english() {
        let en = include_str!("../../../locales/en-US/main.ftl");
        for m in [
            "unterminated string",
            "invalid \\u escape",
            "expected ':'",
            "expected ',' or a closing bracket",
            "too deeply nested to format",
        ] {
            let id = super::json_reason_id(m);
            assert!(en.contains(&format!("{id} = ")), "{id} missing");
        }
    }
}

#[cfg(test)]
mod tab_strip_tests {
    use super::*;

    struct Harness {
        shell: Shell,
        ctx: egui::Context,
        time: f64,
        width: f32,
    }

    impl Harness {
        fn new(width: f32) -> Self {
            let shell = Shell::new(
                Workspace::new(None),
                Localizer::new(&["en-US"]).unwrap(),
                Vec::new(),
                0,
                None,
                Settings::default(),
                None,
            );
            let mut h = Self {
                shell,
                ctx: egui::Context::default(),
                time: 0.0,
                width,
            };
            h.frame(Vec::new());
            h
        }

        fn frame(&mut self, events: Vec<egui::Event>) {
            self.time += 0.05;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(self.width, 300.0),
                )),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            let shell = &mut self.shell;
            let mut out = self.ctx.run_ui(input, |ui| {
                let ctx = ui.ctx().clone();
                egui::Panel::top("tabs").show(ui, |ui| shell.tab_bar(ui, &ctx));
                shell.focus.end_frame(&ctx);
            });
            out.textures_delta.clear();
        }

        fn button(&self, pos: egui::Pos2, pressed: bool) -> egui::Event {
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            }
        }

        fn click(&mut self, pos: egui::Pos2) {
            self.frame(vec![egui::Event::PointerMoved(pos)]);
            self.frame(vec![self.button(pos, true)]);
            self.frame(vec![self.button(pos, false)]);
            // Layout settles (and scrolling applies) a pass later; widget
            // rectangles read back are a pass behind.
            self.frame(Vec::new());
            self.frame(Vec::new());
            self.frame(Vec::new());
        }

        /// The new-tab button: the strip's last keyboard stop.
        fn plus(&self) -> egui::Rect {
            let id = *self.shell.focus.last.tabs.last().unwrap();
            self.ctx.read_response(id).unwrap().rect
        }
    }

    #[test]
    fn plus_button_opens_a_new_tab_after_the_last_one() {
        let mut h = Harness::new(800.0);
        assert_eq!(h.shell.ws.len(), 1);
        // Tab, close button, then "+" right after the tab.
        assert_eq!(h.shell.focus.last.tabs.len(), 3);
        let tab = h
            .ctx
            .read_response(h.shell.focus.last.tabs[0])
            .unwrap()
            .rect;
        let plus = h.plus();
        assert!((plus.left() - tab.right()).abs() < 1.0, "{tab:?} {plus:?}");
        h.click(plus.center());
        assert_eq!(h.shell.ws.len(), 2);
        assert_eq!(h.shell.ws.active_index(), 1);
        assert_eq!(h.shell.focus.last.tabs.len(), 5);
        let plus = h.plus();
        assert!(plus.left() > tab.right() + 10.0, "after the second tab");
        h.click(plus.center());
        assert_eq!(h.shell.ws.len(), 3);
    }

    #[test]
    fn plus_button_stays_in_view_when_tabs_overflow() {
        let mut h = Harness::new(400.0);
        for _ in 0..30 {
            h.shell.ws.new_tab();
        }
        h.frame(Vec::new());
        h.frame(Vec::new());
        h.frame(Vec::new());
        let plus = h.plus();
        assert!(plus.right() <= 400.0 + 0.5, "{plus:?}");
        assert!(plus.left() >= 0.0);
        h.click(plus.center());
        assert_eq!(h.shell.ws.len(), 32);
        // Scrolling is animated.
        for _ in 0..20 {
            h.frame(Vec::new());
        }
        // The new (active) tab is scrolled into view, left of "+".
        let active = h.shell.focus.last.active_tab.unwrap();
        let tab = h.ctx.read_response(active).unwrap().rect;
        let plus = h.plus();
        assert!(tab.right() <= plus.left() + 0.5, "{tab:?} {plus:?}");
        assert!(tab.left() >= 0.0, "{tab:?}");
    }

    #[test]
    fn double_click_on_the_empty_strip_opens_a_new_tab() {
        let mut h = Harness::new(800.0);
        let plus = h.plus();
        let empty = egui::pos2(700.0, plus.center().y);
        h.click(empty);
        assert_eq!(h.shell.ws.len(), 1, "a single click does nothing");
        h.frame(vec![egui::Event::PointerMoved(empty)]);
        for _ in 0..2 {
            h.frame(vec![h.button(empty, true)]);
            h.frame(vec![h.button(empty, false)]);
        }
        h.frame(Vec::new());
        assert_eq!(h.shell.ws.len(), 2);
    }
}

#[cfg(test)]
mod count_tests {
    use super::*;

    struct Harness {
        shell: Shell,
        ctx: egui::Context,
        time: f64,
    }

    impl Harness {
        fn new(settings: Settings) -> Self {
            let shell = Shell::new(
                Workspace::new(None),
                Localizer::new(&["en-US"]).unwrap(),
                Vec::new(),
                0,
                None,
                settings,
                None,
            );
            let ctx = egui::Context::default();
            ctx.enable_accesskit();
            Self {
                shell,
                ctx,
                time: 0.0,
            }
        }

        /// One frame of the status bar; returns the AccessKit update.
        fn frame(&mut self, events: Vec<egui::Event>) -> Option<egui::accesskit::TreeUpdate> {
            self.time += 0.05;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 300.0),
                )),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            let shell = &mut self.shell;
            let mut out = self.ctx.run_ui(input, |ui| {
                let ctx = ui.ctx().clone();
                egui::Panel::bottom("status").show(ui, |ui| shell.status_bar(ui));
                shell.focus.end_frame(&ctx);
            });
            out.textures_delta.clear();
            out.platform_output.accesskit_update
        }

        /// Frames until the counts arrive; the count item's spoken label.
        fn settle(&mut self) -> String {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                let update = self.frame(Vec::new());
                let id = *self.shell.focus.last.status.first().unwrap();
                let label = update
                    .and_then(|u| {
                        u.nodes
                            .into_iter()
                            .find(|(n, _)| *n == id.accesskit_id())
                            .and_then(|(_, node)| node.label().map(str::to_owned))
                    })
                    .unwrap_or_default();
                if !label.is_empty() && !label.contains("not counted")
                    || std::time::Instant::now() > deadline
                {
                    return label;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }

    #[test]
    fn status_bar_shows_document_and_selection_counts() {
        let mut h = Harness::new(Settings::default());
        h.shell
            .editor_mut()
            .replace_range(0..0, "Hello world,\r\nhow are you today?\r\n");
        let label = h.settle();
        assert_eq!(label, "Characters: 30 · Words: 6");
        // The count is the first status bar stop (encoding, line endings,
        // language and count: no indentation detected in this text).
        assert_eq!(h.shell.focus.last.status.len(), 4);
        h.shell.editor_mut().select(0..5);
        let label = h.settle();
        assert_eq!(label, "Characters: 5 of 30 · Words: 1 of 6");
    }

    #[test]
    fn large_numbers_are_grouped() {
        let mut h = Harness::new(Settings::default());
        h.shell
            .editor_mut()
            .replace_range(0..0, &"word ".repeat(1500));
        assert_eq!(h.settle(), "Characters: 7,500 · Words: 1,500");
    }

    #[test]
    fn clicking_the_count_opens_the_summary() {
        let mut h = Harness::new(Settings::default());
        h.shell.editor_mut().replace_range(0..0, "one two");
        h.settle();
        let id = *h.shell.focus.last.status.first().unwrap();
        let at = h.ctx.read_response(id).unwrap().rect.center();
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        h.frame(vec![egui::Event::PointerMoved(at)]);
        h.frame(vec![button(true)]);
        h.frame(vec![button(false)]);
        h.frame(Vec::new());
        assert!(h.shell.counts_open, "the summary popup is open");
    }

    #[test]
    fn the_setting_hides_the_count() {
        let settings = Settings {
            show_counts: false,
            ..Settings::default()
        };
        let mut h = Harness::new(settings);
        h.shell.editor_mut().replace_range(0..0, "one two");
        h.frame(Vec::new());
        h.frame(Vec::new());
        // Encoding, line endings and language only.
        assert_eq!(h.shell.focus.last.status.len(), 3);
        assert!(!h.shell.counts_open);
    }
}
