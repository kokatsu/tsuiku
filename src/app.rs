//! Interactive terminal viewer for worktree and commit changes.
//!
//! Startup discovers changed paths and their Git metadata, but does not read
//! every file. The selected file is loaded and classified on a background
//! thread. Text files then go to a second background thread for line matching;
//! binary files are reported without line matching. Completed contents and line
//! diffs are kept in byte-limited caches so revisiting a file is inexpensive.
//!
//! Background results include the file or content identity they belong to. A
//! completed result is displayed only if it still matches the selected file;
//! results from earlier selections may be cached but cannot replace the visible
//! state. Rendering converts only the rows inside the terminal body into
//! ratatui objects.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::asyncstate::{
    AsyncState, HIGHLIGHTER_VERSION, LINE_MODEL_VERSION, LineDiffCacheKey, LineDiffEngineId,
    LineDiffUnavailable, StructuralSkip, SyntaxHighlightCacheKey,
};
use crate::cache::WeightedLru;
use crate::change::{ChangeDiscoverer, ChangeQuery, ChangeStatus, DiffTarget, FileChange};
use crate::compose::RowOverlays;
use crate::discover::{CommitRevision, GixDiscoverer};
use crate::ids::{ContentIdentity, SnapshotId};
use crate::loader::{ContentLoadCoordinator, LoadResult, PreparedContent, PreparedKind};
use crate::path::GitPath;
use crate::resolve::{GixResolver, ResolveError};
use crate::structural::normalize::DifftStatus;
use crate::structural::tempfiles::LanguagePathHint;
use crate::structural_worker::StructuralDiffCoordinator;
use crate::syntax::DEFAULT_THEME;
use crate::syntax_worker::{SideRequest, SyntaxHighlightCoordinator};
use crate::view::build_unified_lines_with_overlay;
use crate::watch::EventBatch;
use crate::watch::runtime::{WatchCoordinator, WatchUpdate};
use crate::worker::LineDiffCoordinator;

// The layout reserves two title rows and one footer row. Scrolling and
// rendering both subtract this value so they agree on the body height.
const CHROME_HEIGHT: u16 = 3;
// A single entry may exceed its budget so even one unusually large selected
// file remains viewable; inserting it evicts older entries.
const CONTENT_CACHE_BYTES: usize = 32 * 1024 * 1024;
const LINE_DIFF_CACHE_BYTES: usize = 16 * 1024 * 1024;
const STRUCTURAL_DIFF_CACHE_BYTES: usize = 16 * 1024 * 1024;
const SYNTAX_CACHE_BYTES: usize = 16 * 1024 * 1024;
// Preserve a useful diff body on an 80-column terminal, and remove the
// sidebar entirely before it would squeeze the diff below 42 columns.
const SIDEBAR_WIDTH: u16 = 30;
const SIDEBAR_MIN_BODY_WIDTH: u16 = 72;
// Metrics are diagnostic and may run for a long session. Bound sample storage
// independently of the number of input events.
const MAX_METRIC_SAMPLES: usize = 100_000;

/// Failure returned while starting or running the terminal application.
#[derive(Debug)]
pub enum AppError {
    /// Git repository discovery or change enumeration failed.
    Discover(crate::change::DiscoverError),
    /// Terminal setup, input, or drawing failed.
    Io(io::Error),
    /// Standard input or output is not connected to an interactive terminal.
    NotTerminal,
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Discover(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
            Self::NotTerminal => write!(f, "tsuiku requires an interactive terminal"),
        }
    }
}

impl std::error::Error for AppError {}

impl From<io::Error> for AppError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

struct FileModel {
    // Discovery metadata is cheap to retain; file bytes are loaded separately.
    change: FileChange,
    // Some discovery candidates become unchanged after their bytes are
    // resolved. Such entries remain indexed but are skipped by navigation.
    no_op: bool,
    load_error: Option<ResolveError>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FirstContentKind {
    Text,
    Binary,
    SizeLimited,
    Clean,
    LoadError,
    LineDiffError,
}

impl FirstContentKind {
    fn metric_name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Binary => "binary",
            Self::SizeLimited => "size_limited",
            Self::Clean => "clean",
            Self::LoadError => "load_error",
            Self::LineDiffError => "line_diff_error",
        }
    }
}

#[derive(Clone, Copy)]
enum RequestedView<'a> {
    Worktree,
    Show(&'a [u8]),
}

/// Mutable state of one interactive diff-viewer session.
///
/// `App` owns the navigation state, background coordinators, and caches. It
/// does not own terminal restoration; [`Self::run_path`] uses a separate guard
/// so raw mode is also restored after errors and panics.
pub struct App {
    files: Vec<FileModel>,
    /// Generation of `files`. Bumped by every re-discover; background load
    /// results from an older generation are rejected outright because their
    /// file indices may point at different files now.
    snapshot: SnapshotId,
    selected: usize,
    scroll: usize,
    resolver: Option<ContentLoadCoordinator>,
    content_cache: WeightedLru<usize, Arc<PreparedContent>>,
    current_content: Option<Arc<PreparedContent>>,
    worker: LineDiffCoordinator,
    structural_worker: StructuralDiffCoordinator,
    syntax_worker: SyntaxHighlightCoordinator,
    /// Live only in worktree view; `None` for `tsuiku show` (the comparison
    /// is immutable) and after the watcher degrades.
    watch: Option<WatchCoordinator>,
    /// Set once when watching stops working; shown as a title indicator.
    watch_notice: Option<String>,
    comparison_label: Option<String>,
    started_at: Instant,
    terminal_initialized_micros: Cell<Option<u64>>,
    first_content_micros: Cell<Option<u64>>,
    first_content_kind: Cell<Option<FirstContentKind>>,
    navigation_micros: RefCell<Vec<u64>>,
    frame_build_micros: RefCell<Vec<u64>>,
    draw_micros: RefCell<Vec<u64>>,
    metrics_enabled: bool,
}

impl App {
    fn require_terminal() -> Result<(), AppError> {
        if io::stdin().is_terminal() && io::stdout().is_terminal() {
            Ok(())
        } else {
            Err(AppError::NotTerminal)
        }
    }

    fn load(
        path: &Path,
        requested_view: RequestedView<'_>,
        started_at: Instant,
    ) -> Result<Self, AppError> {
        let discoverer = GixDiscoverer::open(path).map_err(AppError::Discover)?;
        let resolved = match requested_view {
            RequestedView::Worktree => None,
            RequestedView::Show(revision) => Some(
                discoverer
                    .resolve_commit_revision(revision)
                    .map_err(AppError::Discover)?,
            ),
        };
        let (target, comparison_label) = requested_target(requested_view, resolved);
        let set = discoverer
            .discover(&ChangeQuery::new(target))
            .map_err(AppError::Discover)?;
        let (repo, paths) = discoverer.into_parts();
        let resolver = GixResolver::from_repository(repo, paths);
        let files = set
            .changes
            .into_iter()
            .map(|change| FileModel {
                change,
                no_op: false,
                load_error: None,
            })
            .collect();
        let mut app = Self {
            files,
            snapshot: SnapshotId(1),
            selected: 0,
            scroll: 0,
            resolver: Some(ContentLoadCoordinator::new(resolver)),
            content_cache: WeightedLru::new(CONTENT_CACHE_BYTES),
            current_content: None,
            worker: LineDiffCoordinator::new(LINE_DIFF_CACHE_BYTES),
            structural_worker: StructuralDiffCoordinator::new(STRUCTURAL_DIFF_CACHE_BYTES),
            syntax_worker: SyntaxHighlightCoordinator::new(SYNTAX_CACHE_BYTES),
            watch: matches!(requested_view, RequestedView::Worktree)
                .then(|| WatchCoordinator::start(path.to_path_buf())),
            watch_notice: None,
            comparison_label,
            started_at,
            terminal_initialized_micros: Cell::new(None),
            first_content_micros: Cell::new(None),
            first_content_kind: Cell::new(None),
            navigation_micros: RefCell::new(Vec::new()),
            frame_build_micros: RefCell::new(Vec::new()),
            draw_micros: RefCell::new(Vec::new()),
            metrics_enabled: std::env::var_os("TSUIKU_METRICS").is_some(),
        };
        app.request_selected();
        Ok(app)
    }

    fn request_selected(&mut self) {
        // State from the previous selection must stop being visible
        // immediately, even if its background work later completes.
        self.scroll = 0;
        self.current_content = None;
        self.worker.reset();
        self.structural_worker.reset();
        self.syntax_worker.reset();
        let Some(file) = self.files.get_mut(self.selected) else {
            return;
        };
        if file.no_op {
            return;
        }
        file.load_error = None;
        if let Some(content) = self.content_cache.get_cloned(&self.selected) {
            self.activate_content(content);
            self.prefetch_adjacent();
            return;
        }
        if let Some(resolver) = &self.resolver {
            resolver.request(self.snapshot, self.selected, file.change.clone());
        }
    }

    /// Replace the discovery snapshot with a newer one.
    ///
    /// The generation is bumped *before* anything else so every in-flight
    /// result of the old generation is already rejectable when the new file
    /// list becomes visible. The content cache is dropped wholesale: its keys
    /// are file indices, which are meaningless across generations, and
    /// content-based reuse is already provided by the line/structural/syntax
    /// caches keyed on `ContentId`. The selection follows its path when the
    /// new snapshot still contains it.
    pub fn apply_snapshot(&mut self, changes: Vec<FileChange>) {
        self.apply_snapshot_with_carry(changes, None);
    }

    /// Like [`Self::apply_snapshot`], optionally transferring the currently
    /// displayed content to the same path in the new snapshot.
    ///
    /// The transfer is deliberate state transfer based on watch-event scope,
    /// not a cache lookup: worktree stamps are hints, never identity, so
    /// only "no observed event could have touched this pair" justifies
    /// skipping the re-read. With a transfer the content worker is not
    /// started at all and the reading position is kept.
    fn apply_snapshot_with_carry(
        &mut self,
        changes: Vec<FileChange>,
        carried: Option<Arc<PreparedContent>>,
    ) {
        let previous_path = self
            .files
            .get(self.selected)
            .map(|file| file.change.display_path().clone());
        self.snapshot = self.snapshot.next();
        self.files = changes
            .into_iter()
            .map(|change| FileModel {
                change,
                no_op: false,
                load_error: None,
            })
            .collect();
        self.content_cache = WeightedLru::new(CONTENT_CACHE_BYTES);
        let repositioned = previous_path.and_then(|path| {
            self.files
                .iter()
                .position(|file| file.change.display_path() == &path)
        });
        self.selected =
            repositioned.unwrap_or_else(|| self.selected.min(self.files.len().saturating_sub(1)));
        match (carried, repositioned) {
            // The selected path survived and nothing touched its content:
            // reactivate the held pair under the new generation. Line,
            // structural and syntax requests re-key by ContentPairId and hit
            // their caches.
            (Some(content), Some(_)) => {
                let scroll = self.scroll;
                self.current_content = None;
                self.worker.reset();
                self.structural_worker.reset();
                self.syntax_worker.reset();
                let weight = content.estimated_bytes();
                self.content_cache
                    .insert(self.selected, Arc::clone(&content), weight);
                self.activate_content(content);
                self.scroll = scroll;
                self.prefetch_adjacent();
            }
            _ => self.request_selected(),
        }
    }

    /// Apply one completed watch update.
    fn handle_watch_update(&mut self, update: WatchUpdate) {
        match update {
            WatchUpdate::Refresh { changes, batch } => self.handle_watch_refresh(changes, batch),
            WatchUpdate::Degraded { reason } => {
                self.watch_notice = Some(reason);
                self.watch = None;
            }
        }
    }

    /// A refreshed snapshot always replaces the file list; whether the
    /// displayed content needs a re-read is decided per selection from the
    /// event batch, conservatively (metadata, ignore-source, unknown and
    /// lossy batches always re-read).
    fn handle_watch_refresh(&mut self, changes: crate::change::ChangeSet, batch: EventBatch) {
        let selection_untouched = self.files.get(self.selected).is_some_and(|file| {
            !batch.affects_selection(file.change.old_path.as_ref(), file.change.new_path.as_ref())
        });
        let carried = if selection_untouched {
            self.current_content.clone()
        } else {
            None
        };
        let scroll = self.scroll;
        self.apply_snapshot_with_carry(changes.changes, carried);
        // Keep the reading position where possible; the viewport build
        // clamps an offset past the new row count.
        self.scroll = scroll;
    }

    fn activate_content(&mut self, content: Arc<PreparedContent>) {
        match content.kind {
            PreparedKind::NoOp => return,
            PreparedKind::Binary => self.worker.skip(LineDiffUnavailable::Binary),
            PreparedKind::Text => {
                let key = LineDiffCacheKey {
                    pair: content.pair,
                    engine: LineDiffEngineId::Imara,
                    options_fingerprint: 0,
                    line_model_version: LINE_MODEL_VERSION,
                };
                self.worker.request(
                    key,
                    Arc::clone(content.old.as_ref().expect("text old side")),
                    Arc::clone(content.new.as_ref().expect("text new side")),
                );
                let change = &self.files[self.selected].change;
                let old_hint = change
                    .old_path
                    .as_ref()
                    .map(LanguagePathHint::from_git_path)
                    .unwrap_or_else(LanguagePathHint::none);
                let new_hint = change
                    .new_path
                    .as_ref()
                    .map(LanguagePathHint::from_git_path)
                    .unwrap_or_else(LanguagePathHint::none);
                self.structural_worker.request(
                    content.pair,
                    old_hint.clone(),
                    new_hint.clone(),
                    Arc::clone(content.old.as_ref().expect("text old side")),
                    Arc::clone(content.new.as_ref().expect("text new side")),
                );
                self.syntax_worker.request(
                    syntax_side(content.pair.old, old_hint, content.old.as_ref()),
                    syntax_side(content.pair.new, new_hint, content.new.as_ref()),
                );
            }
        }
        self.current_content = Some(content);
    }

    fn handle_load_result(&mut self, result: LoadResult) {
        // A stale-generation result may describe a different file than the
        // one now occupying its index; it must neither display nor cache.
        if result.snapshot != self.snapshot {
            return;
        }
        let selected = result.file_id == self.selected;
        match result.result {
            Ok(content) if content.kind == PreparedKind::NoOp => {
                // Discovery can conservatively report a candidate whose
                // resolved sides are identical. Remove it from navigation.
                if let Some(file) = self.files.get_mut(result.file_id) {
                    file.no_op = true;
                }
                if selected {
                    if let Some(next) = next_visible(&self.files, self.selected, true)
                        .or_else(|| next_visible(&self.files, self.selected, false))
                    {
                        self.selected = next;
                        self.request_selected();
                    } else {
                        self.current_content = None;
                        self.worker.reset();
                        self.structural_worker.reset();
                        self.syntax_worker.reset();
                    }
                } else {
                    // The prefetched candidate disappeared from navigation.
                    // Continue through no-op candidates until the selected
                    // file has one genuinely visible neighbor warmed.
                    self.prefetch_adjacent();
                }
            }
            Ok(content) => {
                let content = Arc::new(content);
                let weight = content.estimated_bytes();
                self.content_cache
                    .insert(result.file_id, Arc::clone(&content), weight);
                if selected {
                    self.activate_content(content);
                    self.prefetch_adjacent();
                }
            }
            Err(error) => {
                if let Some(file) = self.files.get_mut(result.file_id) {
                    file.load_error = Some(error);
                }
                if selected {
                    self.prefetch_adjacent();
                }
            }
        }
    }

    fn prefetch_adjacent(&self) {
        let Some(file_id) = adjacent_prefetch_candidate(&self.files, self.selected, |candidate| {
            self.content_cache.contains_key(&candidate)
        }) else {
            return;
        };
        if let (Some(resolver), Some(file)) = (&self.resolver, self.files.get(file_id)) {
            resolver.prefetch(self.snapshot, file_id, file.change.clone());
        }
    }

    fn poll_workers(&mut self) -> bool {
        // Line-diff results perform their own cache-key check before becoming
        // visible. Content results use file_id for the equivalent check below.
        let mut dirty = self.worker.poll();
        dirty |= self.structural_worker.poll();
        dirty |= self.syntax_worker.poll();
        loop {
            let update = self.watch.as_ref().and_then(WatchCoordinator::poll);
            let Some(update) = update else {
                break;
            };
            dirty = true;
            self.handle_watch_update(update);
        }
        loop {
            let result = self
                .resolver
                .as_ref()
                .and_then(|resolver| resolver.try_recv());
            let Some(result) = result else {
                break;
            };
            dirty = true;
            self.handle_load_result(result);
        }
        dirty
    }

    fn next_file(&mut self) {
        if let Some(next) = next_visible(&self.files, self.selected, true) {
            self.selected = next;
            self.request_selected();
        }
    }

    fn previous_file(&mut self) {
        if let Some(previous) = next_visible(&self.files, self.selected, false) {
            self.selected = previous;
            self.request_selected();
        }
    }

    fn max_scroll(&self, viewport_height: usize) -> usize {
        match self.worker.state() {
            AsyncState::Ready(rows) => max_scroll_for_rows(rows.len(), viewport_height),
            _ => 0,
        }
    }

    /// Loads and runs the application while showing immediate startup feedback.
    ///
    /// The terminal is initialized and a “Discovering changes…” frame is drawn
    /// before repository discovery. This makes the interface responsive and,
    /// when `TSUIKU_METRICS` is set, measures the complete user-visible startup
    /// interval.
    pub fn run_path(path: &Path) -> Result<(), AppError> {
        Self::run_requested(path, RequestedView::Worktree)
    }

    /// Show a commit against its first parent, or against the empty tree when
    /// `revision` names a root commit.
    pub fn run_show(path: &Path, revision: &[u8]) -> Result<(), AppError> {
        Self::run_requested(path, RequestedView::Show(revision))
    }

    fn run_requested(path: &Path, requested_view: RequestedView<'_>) -> Result<(), AppError> {
        Self::require_terminal()?;
        let started_at = Instant::now();
        install_panic_hook();
        let guard = TerminalGuard::enter()?;
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| {
            frame.render_widget(Paragraph::new("Discovering changes…"), frame.area());
        })?;
        let terminal_initialized = elapsed_micros(started_at);
        let mut app = Self::load(path, requested_view, started_at)?;
        app.terminal_initialized_micros
            .set(Some(terminal_initialized));
        let result = app.event_loop(&mut terminal);
        // Restore the terminal before dropping `app`, whose optional metrics
        // are printed to the normal screen on stderr.
        drop(terminal);
        drop(guard);
        drop(app);
        result
    }

    fn event_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<(), AppError> {
        let mut dirty = true;
        let mut terminal_area = terminal.size()?;
        loop {
            dirty |= self.poll_workers();
            if dirty {
                let build_started = Instant::now();
                let body_height = body_height(terminal_area.height);
                let title = self.title();
                let body = self.body_lines(body_height);
                let sidebar = self.sidebar_for_body(body_height, terminal_area.width);
                let content_kind = self.visible_content_kind();
                record_metric(
                    self.metrics_enabled,
                    &self.frame_build_micros,
                    elapsed_micros(build_started),
                );
                let draw_started = Instant::now();
                terminal.draw(|frame| self.draw(frame, title, body, sidebar))?;
                record_metric(
                    self.metrics_enabled,
                    &self.draw_micros,
                    elapsed_micros(draw_started),
                );
                if let Some(kind) = content_kind
                    && self.first_content_micros.get().is_none()
                {
                    self.first_content_micros
                        .set(Some(elapsed_micros(self.started_at)));
                    self.first_content_kind.set(Some(kind));
                }
                dirty = false;
            }

            if !event::poll(Duration::from_millis(16))? {
                continue;
            }
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let navigation_started = Instant::now();
                    let changed = self.handle_key(key, body_height(terminal_area.height));
                    record_metric(
                        self.metrics_enabled,
                        &self.navigation_micros,
                        elapsed_micros(navigation_started),
                    );
                    if !changed {
                        continue;
                    }
                    if is_quit_key(key) {
                        return Ok(());
                    }
                    dirty = true;
                }
                Event::Resize(width, height) => {
                    terminal_area = self.handle_resize(width, height);
                    dirty = true;
                }
                _ => {}
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent, viewport_height: usize) -> bool {
        if is_quit_key(key) {
            return true;
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.scroll = (self.scroll + 1).min(self.max_scroll(viewport_height));
                true
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                true
            }
            KeyCode::PageDown => {
                self.scroll = (self.scroll + viewport_height).min(self.max_scroll(viewport_height));
                true
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(viewport_height);
                true
            }
            KeyCode::Char(']') => {
                if let Some(target) = next_hunk_offset(self.worker.hunk_starts(), self.scroll) {
                    self.scroll = target.min(self.max_scroll(viewport_height));
                }
                true
            }
            KeyCode::Char('[') => {
                if let Some(target) = previous_hunk_offset(self.worker.hunk_starts(), self.scroll) {
                    self.scroll = target.min(self.max_scroll(viewport_height));
                }
                true
            }
            KeyCode::Char('n') => {
                self.next_file();
                true
            }
            KeyCode::Char('p') => {
                self.previous_file();
                true
            }
            _ => false,
        }
    }

    fn handle_resize(&mut self, width: u16, height: u16) -> Size {
        // Reuse the event size for drawing and key handling instead of querying
        // the terminal per key press. A taller body lowers the maximum valid
        // scroll offset, so clamp before drawing the resized frame.
        let area = Size { width, height };
        self.scroll = self.scroll.min(self.max_scroll(body_height(height)));
        area
    }

    fn draw<'a>(
        &'a self,
        frame: &mut ratatui::Frame<'_>,
        title: String,
        body: Vec<Line<'a>>,
        sidebar: Option<Vec<Line<'static>>>,
    ) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(frame.area());

        frame.render_widget(
            Paragraph::new(title)
                .style(Style::default().add_modifier(Modifier::BOLD))
                .block(Block::default().borders(Borders::BOTTOM)),
            chunks[0],
        );
        let (sidebar_area, content) = body_areas(chunks[1], sidebar.is_some());
        if let (Some(sidebar_area), Some(sidebar)) = (sidebar_area, sidebar) {
            frame.render_widget(
                Paragraph::new(sidebar).block(Block::default().borders(Borders::RIGHT)),
                sidebar_area,
            );
        }
        frame.render_widget(Paragraph::new(body), content);
        frame.render_widget(
            Paragraph::new(" j/k scroll  [/] hunk  n/p file  PgUp/PgDn  q/Ctrl-C quit ")
                .style(Style::default().fg(Color::DarkGray)),
            chunks[2],
        );
    }

    fn sidebar_for_body(&self, height: usize, body_width: u16) -> Option<Vec<Line<'static>>> {
        (self.visible_file_count() > 0 && body_width >= SIDEBAR_MIN_BODY_WIDTH)
            .then(|| self.sidebar_lines(height, SIDEBAR_WIDTH))
    }

    fn body_lines(&self, height: usize) -> Vec<Line<'_>> {
        if self.visible_file_count() == 0 {
            return vec![Line::from(self.no_changes_message())];
        }
        let Some(file) = self.files.get(self.selected) else {
            return vec![Line::from(self.no_changes_message())];
        };
        if let Some(error) = &file.load_error {
            return vec![Line::from(format!("Cannot read file: {error}"))];
        }
        let Some(content) = &self.current_content else {
            return vec![Line::from("Loading content…")];
        };
        match self.worker.state() {
            AsyncState::NotRequested | AsyncState::Pending { .. } => {
                vec![Line::from("Computing line diff…")]
            }
            AsyncState::Skipped(LineDiffUnavailable::Binary) => {
                vec![Line::from("Binary files differ")]
            }
            AsyncState::Skipped(LineDiffUnavailable::SizeLimited { bytes, limit }) => {
                vec![Line::from(format!(
                    "File too large to diff ({bytes} bytes; limit {limit})"
                ))]
            }
            AsyncState::Failed(error) => vec![Line::from(format!("Line diff failed: {error:?}"))],
            AsyncState::Ready(rows) => build_unified_lines_with_overlay(
                rows,
                content.old.as_ref().expect("ready old text"),
                content.new.as_ref().expect("ready new text"),
                RowOverlays {
                    structural: match self.structural_worker.state() {
                        AsyncState::Ready(overlay) => Some(overlay.as_ref()),
                        _ => None,
                    },
                    syntax_old: match self.syntax_worker.old_state() {
                        AsyncState::Ready(spans) => Some(spans.as_ref()),
                        _ => None,
                    },
                    syntax_new: match self.syntax_worker.new_state() {
                        AsyncState::Ready(spans) => Some(spans.as_ref()),
                        _ => None,
                    },
                },
                // A scroll preserved across a watch refresh may exceed the
                // new diff; clamp for display, keys clamp the stored value.
                self.scroll.min(max_scroll_for_rows(rows.len(), height)),
                height,
            ),
        }
    }

    fn sidebar_lines(&self, height: usize, width: u16) -> Vec<Line<'static>> {
        // The right border consumes one column inside the sidebar area.
        let line_width = width.saturating_sub(1) as usize;
        sidebar_file_indices(&self.files, self.selected, height)
            .into_iter()
            .map(|index| {
                let selected = index == self.selected;
                let prefix = if selected { "> " } else { "  " };
                let label = file_label(&self.files[index].change);
                let mut text = format!(
                    "{prefix}{}",
                    truncate_display_width(&label, line_width.saturating_sub(prefix.len()))
                );
                if selected {
                    let padding = line_width.saturating_sub(UnicodeWidthStr::width(text.as_str()));
                    text.push_str(&" ".repeat(padding));
                }
                let style = if selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                Line::styled(text, style)
            })
            .collect()
    }

    fn visible_file_count(&self) -> usize {
        self.files.iter().filter(|file| !file.no_op).count()
    }

    fn no_changes_message(&self) -> &'static str {
        if self.comparison_label.is_some() {
            "No changes in this comparison."
        } else {
            "Working tree is clean."
        }
    }

    fn title(&self) -> String {
        let prefix = match &self.comparison_label {
            Some(label) => format!(" tsuiku  {label}  "),
            None => " tsuiku  ".to_owned(),
        };
        // The stop reason (inotify limits, a lost repository, …) is what the
        // user needs in order to react; it comes from error strings that can
        // embed file names, so it must be escaped before reaching the
        // terminal. Overlong reasons are clipped by the terminal width.
        let watch = match &self.watch_notice {
            Some(reason) => format!("  watch: off ({})", terminal_safe_label(reason)),
            None => String::new(),
        };
        let visible_count = self.visible_file_count();
        if visible_count == 0 {
            // A start failure on a clean tree must stay visible too.
            return format!("{prefix}no changes{watch} ");
        }
        let Some(file) = self.files.get(self.selected) else {
            return format!("{prefix}no changes{watch} ");
        };
        let ordinal = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, file)| !file.no_op)
            .position(|(id, _)| id == self.selected)
            .map_or(0, |index| index + 1);
        let structural = match self.structural_worker.state() {
            AsyncState::NotRequested => "",
            AsyncState::Pending { .. } => "  structural: pending",
            // Difftastic found no structural difference at all — the change
            // is formatting noise. Say so explicitly rather than showing an
            // empty span count, which reads as "nothing was highlighted".
            AsyncState::Ready(overlay) if overlay.status == DifftStatus::Unchanged => {
                "  structural: no structural change"
            }
            AsyncState::Ready(overlay) => {
                return format!(
                    "{prefix}[{ordinal}/{visible_count}] {}  structural: {} {}/{}{watch} ",
                    file_label(&file.change),
                    terminal_safe_label(&overlay.language),
                    overlay.diagnostics.accepted,
                    overlay.diagnostics.total
                );
            }
            AsyncState::Skipped(StructuralSkip::ToolUnavailable) => "  structural: unavailable",
            AsyncState::Skipped(StructuralSkip::SizeLimited) => "  structural: size limited",
            AsyncState::Skipped(StructuralSkip::UnsupportedLanguage) => "  structural: unsupported",
            AsyncState::Skipped(StructuralSkip::IncompatibleVersion) => {
                "  structural: incompatible"
            }
            // The A/D marker already says why; keep the title short.
            AsyncState::Skipped(StructuralSkip::OneSided) => "  structural: n/a",
            AsyncState::Failed(_) => "  structural: failed",
        };
        format!(
            "{prefix}[{ordinal}/{visible_count}] {}{structural}{watch} ",
            file_label(&file.change),
        )
    }

    fn visible_content_kind(&self) -> Option<FirstContentKind> {
        if self.visible_file_count() == 0 {
            return Some(FirstContentKind::Clean);
        }
        let file = self.files.get(self.selected)?;
        if file.load_error.is_some() {
            return Some(FirstContentKind::LoadError);
        }
        self.current_content.as_ref()?;
        match self.worker.state() {
            AsyncState::Ready(_) => Some(FirstContentKind::Text),
            AsyncState::Skipped(LineDiffUnavailable::Binary) => Some(FirstContentKind::Binary),
            AsyncState::Skipped(LineDiffUnavailable::SizeLimited { .. }) => {
                Some(FirstContentKind::SizeLimited)
            }
            AsyncState::Failed(_) => Some(FirstContentKind::LineDiffError),
            AsyncState::NotRequested | AsyncState::Pending { .. } => None,
        }
    }
}

/// The syntax request for one side, or `None` for an absent side (add or
/// delete), which stays `NotRequested`.
fn syntax_side(
    identity: ContentIdentity,
    hint: LanguagePathHint,
    text: Option<&Arc<crate::text::TextContent>>,
) -> Option<SideRequest> {
    let ContentIdentity::Present(content) = identity else {
        return None;
    };
    Some(SideRequest {
        key: SyntaxHighlightCacheKey {
            content,
            language_hint: hint,
            theme_id: DEFAULT_THEME,
            highlighter_version: HIGHLIGHTER_VERSION,
            options_fingerprint: 0,
        },
        text: Arc::clone(text.expect("text side present")),
    })
}

fn requested_target(
    requested_view: RequestedView<'_>,
    resolved: Option<CommitRevision>,
) -> (DiffTarget, Option<String>) {
    match (requested_view, resolved) {
        (RequestedView::Worktree, None) => (DiffTarget::WorktreeVsHead, None),
        (RequestedView::Show(revision), Some(resolved)) => (
            DiffTarget::CommitVsParent {
                commit: resolved.commit,
            },
            Some(commit_comparison_label(revision, resolved.has_parent)),
        ),
        _ => unreachable!("only show revisions have resolved commit metadata"),
    }
}

fn commit_comparison_label(revision: &[u8], has_parent: bool) -> String {
    let revision = GitPath::from_bytes(revision).display_escaped();
    if has_parent {
        format!("comparing {revision}^1..{revision}")
    } else {
        format!("comparing empty..{revision}")
    }
}

fn next_visible(files: &[FileModel], selected: usize, forward: bool) -> Option<usize> {
    if forward {
        ((selected + 1)..files.len()).find(|&index| !files[index].no_op)
    } else {
        (0..selected).rev().find(|&index| !files[index].no_op)
    }
}

fn adjacent_prefetch_candidate(
    files: &[FileModel],
    selected: usize,
    is_cached: impl Fn(usize) -> bool,
) -> Option<usize> {
    [
        next_visible(files, selected, true),
        next_visible(files, selected, false),
    ]
    .into_iter()
    .flatten()
    .find(|&candidate| !is_cached(candidate))
}

fn body_height(terminal_height: u16) -> usize {
    terminal_height.saturating_sub(CHROME_HEIGHT) as usize
}

fn body_areas(area: Rect, show_sidebar: bool) -> (Option<Rect>, Rect) {
    if !show_sidebar || area.width < SIDEBAR_MIN_BODY_WIDTH {
        return (None, area);
    }
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(1)])
        .split(area);
    (Some(chunks[0]), chunks[1])
}

fn sidebar_file_indices(files: &[FileModel], selected: usize, height: usize) -> Vec<usize> {
    if height == 0 || files.get(selected).is_none_or(|file| file.no_op) {
        return Vec::new();
    }

    let mut start = selected;
    for _ in 0..height / 2 {
        let Some(previous) = next_visible(files, start, false) else {
            break;
        };
        start = previous;
    }

    let mut indices = VecDeque::with_capacity(height);
    let mut cursor = Some(start);
    while let Some(index) = cursor
        && indices.len() < height
    {
        indices.push_back(index);
        cursor = next_visible(files, index, true);
    }

    while indices.len() < height {
        let Some(previous) = next_visible(files, indices[0], false) else {
            break;
        };
        indices.push_front(previous);
    }
    indices.into_iter().collect()
}

fn truncate_display_width(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_owned();
    }

    let content_width = max_width - 1;
    let mut result = String::new();
    let mut width = 0;
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width > content_width {
            break;
        }
        result.push(character);
        width += character_width;
    }
    result.push('…');
    result
}

fn max_scroll_for_rows(rows: usize, viewport_height: usize) -> usize {
    rows.saturating_sub(viewport_height)
}

fn next_hunk_offset(hunk_starts: &[usize], scroll: usize) -> Option<usize> {
    hunk_starts
        .get(hunk_starts.partition_point(|&start| start <= scroll))
        .copied()
}

fn previous_hunk_offset(hunk_starts: &[usize], scroll: usize) -> Option<usize> {
    let before = hunk_starts.partition_point(|&start| start < scroll);
    before
        .checked_sub(1)
        .and_then(|index| hunk_starts.get(index))
        .copied()
}

fn is_quit_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Char('q')
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}

fn file_label(change: &FileChange) -> String {
    let status = match change.status {
        ChangeStatus::Add => "A",
        ChangeStatus::Delete => "D",
        ChangeStatus::Modify => "M",
        ChangeStatus::Rename => "R",
    };
    let path = match (&change.old_path, &change.new_path) {
        (Some(old), Some(new)) if old != new => {
            format!("{} → {}", old.display_escaped(), new.display_escaped())
        }
        _ => change.display_path().display_escaped(),
    };
    if change.old_mode != change.new_mode {
        format!(
            "{status} {path} [{:?} → {:?}]",
            change.old_mode, change.new_mode
        )
    } else {
        format!("{status} {path}")
    }
}

fn terminal_safe_label(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| {
            if character.is_control() {
                character.escape_default().collect::<Vec<_>>()
            } else {
                vec![character]
            }
        })
        .collect()
}

fn elapsed_micros(started: Instant) -> u64 {
    started.elapsed().as_micros().min(u64::MAX as u128) as u64
}

fn record_metric(enabled: bool, samples: &RefCell<Vec<u64>>, value: u64) {
    if !enabled {
        return;
    }
    let mut samples = samples.borrow_mut();
    if samples.len() < MAX_METRIC_SAMPLES {
        samples.push(value);
    }
}

fn percentile_95(input: &[u64]) -> Option<u64> {
    let mut samples = input.to_vec();
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    Some(samples[(samples.len() - 1) * 95 / 100])
}

/// Restores normal terminal mode when the interactive session ends.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self, AppError> {
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(AppError::Io(error));
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
}

fn install_panic_hook() {
    static INSTALL: Once = Once::new();
    let main_thread = std::thread::current().id();
    INSTALL.call_once(move || {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Only the main thread owns the interactive terminal. A detached
            // worker panic must not tear down a still-running UI.
            if std::thread::current().id() == main_thread {
                restore_terminal();
            }
            previous(info);
        }));
    });
}

impl Drop for App {
    fn drop(&mut self) {
        if !self.metrics_enabled {
            return;
        }
        if let Some(value) = self.terminal_initialized_micros.get() {
            eprintln!("METRIC terminal_initialized_us={value}");
        }
        if let Some(value) = self.first_content_micros.get() {
            eprintln!("METRIC first_content_visible_us={value}");
        }
        if let Some(kind) = self.first_content_kind.get() {
            eprintln!("METRIC first_content_kind={}", kind.metric_name());
        }
        if let Some(value) = percentile_95(self.navigation_micros.get_mut()) {
            eprintln!("METRIC navigation_state_update_p95_us={value}");
        }
        if let Some(value) = percentile_95(self.frame_build_micros.get_mut()) {
            eprintln!("METRIC visible_frame_build_p95_us={value}");
        }
        if let Some(value) = percentile_95(self.draw_micros.get_mut()) {
            eprintln!("METRIC backend_draw_p95_us={value}");
        }
        eprintln!(
            "METRIC content_cache_bytes={}",
            self.content_cache.total_weight()
        );
        eprintln!(
            "METRIC line_diff_cache_bytes={}",
            self.worker.cache_weight()
        );
        eprintln!(
            "METRIC structural_diff_cache_bytes={}",
            self.structural_worker.cache_weight()
        );
        eprintln!(
            "METRIC syntax_cache_bytes={}",
            self.syntax_worker.cache_weight()
        );
        let (accepted, total) = self.structural_worker.diagnostic_totals();
        eprintln!("METRIC structural_spans_accepted={accepted}");
        eprintln!("METRIC structural_spans_total={total}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::EntryMode;
    use crate::ids::{ContentIdentity, ContentPairId};
    use crate::ids::{ContentSource, Oid};
    use crate::loader::LoadPriority;
    use crate::path::GitPath;
    use crate::text::{ClassifiedContent, classify};
    use ratatui::backend::TestBackend;

    fn change(status_path: &[u8]) -> FileChange {
        FileChange::classify(
            None,
            Some(GitPath::from_bytes(status_path)),
            ContentSource::Absent,
            ContentSource::Submodule {
                commit: Oid([1; 20]),
                dirty: false,
            },
            None,
            Some(EntryMode::Submodule),
        )
        .expect("change")
    }

    fn test_app() -> App {
        App {
            files: vec![
                FileModel {
                    change: change(b"a"),
                    no_op: false,
                    load_error: None,
                },
                FileModel {
                    change: change(b"b"),
                    no_op: false,
                    load_error: None,
                },
            ],
            snapshot: SnapshotId(1),
            selected: 0,
            scroll: 0,
            resolver: None,
            content_cache: WeightedLru::new(1024 * 1024),
            current_content: None,
            worker: LineDiffCoordinator::new(1024 * 1024),
            structural_worker: StructuralDiffCoordinator::new(1024 * 1024),
            syntax_worker: SyntaxHighlightCoordinator::new(1024 * 1024),
            watch: None,
            watch_notice: None,
            comparison_label: None,
            started_at: Instant::now(),
            terminal_initialized_micros: Cell::new(None),
            first_content_micros: Cell::new(None),
            first_content_kind: Cell::new(None),
            navigation_micros: RefCell::new(Vec::new()),
            frame_build_micros: RefCell::new(Vec::new()),
            draw_micros: RefCell::new(Vec::new()),
            metrics_enabled: false,
        }
    }

    fn test_app_with_prefetch() -> App {
        let mut app = test_app();
        app.files.push(FileModel {
            change: change(b"c"),
            no_op: false,
            load_error: None,
        });
        app.resolver = Some(ContentLoadCoordinator::new_for_test());
        app
    }

    fn text_content(source: &str) -> Arc<crate::text::TextContent> {
        match classify(Arc::from(source.as_bytes())) {
            ClassifiedContent::Text(text) => Arc::new(text),
            ClassifiedContent::Binary(_) => panic!("text fixture"),
        }
    }

    fn prepared_value(kind: PreparedKind) -> PreparedContent {
        let old = text_content("old\n");
        let new = text_content("new\n");
        PreparedContent {
            pair: ContentPairId {
                old: ContentIdentity::Present(crate::ids::ContentId::compute(b"old\n")),
                new: ContentIdentity::Present(crate::ids::ContentId::compute(b"new\n")),
            },
            kind,
            old: (kind == PreparedKind::Text).then_some(old),
            new: (kind == PreparedKind::Text).then_some(new),
        }
    }

    fn prepared(kind: PreparedKind) -> Arc<PreparedContent> {
        Arc::new(prepared_value(kind))
    }

    fn prepared_text(old_source: &str, new_source: &str) -> Arc<PreparedContent> {
        Arc::new(PreparedContent {
            pair: ContentPairId {
                old: ContentIdentity::Present(crate::ids::ContentId::compute(
                    old_source.as_bytes(),
                )),
                new: ContentIdentity::Present(crate::ids::ContentId::compute(
                    new_source.as_bytes(),
                )),
            },
            kind: PreparedKind::Text,
            old: Some(text_content(old_source)),
            new: Some(text_content(new_source)),
        })
    }

    fn load_error(path: &str, message: &str) -> ResolveError {
        ResolveError::Io {
            path: path.to_owned(),
            source: io::Error::new(io::ErrorKind::NotFound, message),
        }
    }

    fn wait_for_line_diff(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !matches!(app.worker.state(), AsyncState::Ready(_)) {
            app.worker.poll();
            assert!(Instant::now() < deadline, "line diff did not finish");
            std::thread::yield_now();
        }
    }

    fn wait_for_syntax(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !matches!(app.syntax_worker.new_state(), AsyncState::Ready(_)) {
            app.syntax_worker.poll();
            assert!(Instant::now() < deadline, "syntax highlight did not finish");
            std::thread::yield_now();
        }
    }

    #[test]
    fn ready_syntax_highlight_colors_the_visible_body() {
        let mut app = test_app();
        app.files[0].change.new_path = Some(GitPath::from_bytes(b"a.rs"));
        app.files[0].change.old_path = Some(GitPath::from_bytes(b"a.rs"));
        app.activate_content(prepared_text(
            "fn old() -> u32 { 1 } // note\n",
            "fn new() -> u32 { 2 } // note\n",
        ));
        wait_for_line_diff(&mut app);
        wait_for_syntax(&mut app);

        let has_rgb_fg = app.body_lines(10).iter().any(|line| {
            line.spans
                .iter()
                .any(|span| matches!(span.style.fg, Some(Color::Rgb(..))))
        });
        assert!(has_rgb_fg, "a ready highlight must color at least one span");
    }

    #[test]
    fn viewport_height_and_scroll_use_the_same_chrome_height() {
        assert_eq!(body_height(24), 21);
        assert_eq!(max_scroll_for_rows(100, body_height(24)), 79);
        assert_eq!(max_scroll_for_rows(10, body_height(24)), 0);
    }

    #[test]
    fn sidebar_disappears_before_it_squeezes_the_diff_body() {
        let narrow = Rect::new(3, 4, SIDEBAR_MIN_BODY_WIDTH - 1, 20);
        assert_eq!(body_areas(narrow, true), (None, narrow));

        let wide = Rect::new(3, 4, SIDEBAR_MIN_BODY_WIDTH, 20);
        let (sidebar, content) = body_areas(wide, true);
        assert_eq!(sidebar, Some(Rect::new(3, 4, SIDEBAR_WIDTH, 20)));
        assert_eq!(
            content,
            Rect::new(
                3 + SIDEBAR_WIDTH,
                4,
                SIDEBAR_MIN_BODY_WIDTH - SIDEBAR_WIDTH,
                20
            )
        );
        assert_eq!(body_areas(wide, false), (None, wide));
    }

    #[test]
    fn sidebar_window_skips_no_ops_and_keeps_the_selection_visible() {
        let files = (0..7)
            .map(|index| FileModel {
                change: change(format!("{index}").as_bytes()),
                no_op: index == 2,
                load_error: None,
            })
            .collect::<Vec<_>>();

        assert_eq!(sidebar_file_indices(&files, 4, 3), vec![3, 4, 5]);
        assert_eq!(sidebar_file_indices(&files, 6, 3), vec![4, 5, 6]);
        assert!(sidebar_file_indices(&files, 4, 0).is_empty());
    }

    #[test]
    fn display_width_truncation_handles_wide_characters() {
        assert_eq!(truncate_display_width("日本語", 5), "日本…");
        assert_eq!(UnicodeWidthStr::width("日本…"), 5);
        assert_eq!(truncate_display_width("abc", 3), "abc");
        assert_eq!(truncate_display_width("abc", 1), "…");
        assert_eq!(truncate_display_width("abc", 0), "");
    }

    #[test]
    fn sidebar_lines_mark_and_pad_the_selection() {
        let mut app = test_app();
        app.files[0].change.new_mode = None;
        let lines = app.sidebar_lines(2, 10);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].spans[0].content.starts_with("> "));
        assert_eq!(
            UnicodeWidthStr::width(lines[0].spans[0].content.as_ref()),
            9
        );
        assert_eq!(lines[0].style.bg, Some(Color::LightCyan));
        assert!(lines[1].spans[0].content.starts_with("  "));
    }

    #[test]
    fn wide_draw_renders_sidebar_and_narrow_draw_gives_the_body_full_width() {
        let mut app = test_app();
        app.files[0].change.new_mode = None;
        let mut wide = Terminal::new(TestBackend::new(80, 8)).expect("test terminal");
        let wide_sidebar = app.sidebar_for_body(body_height(8), 80);
        wide.draw(|frame| {
            app.draw(
                frame,
                app.title(),
                vec![Line::from("diff body")],
                wide_sidebar,
            )
        })
        .expect("wide draw");
        let wide_buffer = wide.backend().buffer();
        assert_eq!(
            wide_buffer.cell((0, 2)).expect("selected marker").symbol(),
            ">"
        );
        assert_eq!(
            wide_buffer.cell((29, 2)).expect("sidebar border").symbol(),
            "│"
        );
        assert_eq!(
            wide_buffer.cell((28, 2)).expect("padded selection").bg,
            Color::LightCyan
        );
        assert_eq!(wide_buffer.cell((30, 2)).expect("diff body").symbol(), "d");

        let mut narrow = Terminal::new(TestBackend::new(71, 8)).expect("test terminal");
        let narrow_sidebar = app.sidebar_for_body(body_height(8), 71);
        narrow
            .draw(|frame| {
                app.draw(
                    frame,
                    app.title(),
                    vec![Line::from("diff body")],
                    narrow_sidebar,
                )
            })
            .expect("narrow draw");
        let narrow_buffer = narrow.backend().buffer();
        assert_eq!(narrow_buffer.cell((0, 2)).expect("diff body").symbol(), "d");
        assert_ne!(
            narrow_buffer
                .cell((29, 2))
                .expect("no sidebar border")
                .symbol(),
            "│"
        );
    }

    #[test]
    fn hunk_offsets_stop_at_edges_and_return_to_the_current_hunk_start() {
        let starts = [1, 5, 12];
        assert_eq!(next_hunk_offset(&starts, 0), Some(1));
        assert_eq!(next_hunk_offset(&starts, 1), Some(5));
        assert_eq!(next_hunk_offset(&starts, 12), None);
        assert_eq!(previous_hunk_offset(&starts, 7), Some(5));
        assert_eq!(previous_hunk_offset(&starts, 5), Some(1));
        assert_eq!(previous_hunk_offset(&starts, 1), None);
        assert_eq!(previous_hunk_offset(&starts, 0), None);
    }

    #[test]
    fn hunk_past_the_last_page_clamps_to_a_viewport_that_contains_it() {
        let starts = [10, 90, 95];
        let rows = 100;
        let viewport = 20;
        let max_scroll = max_scroll_for_rows(rows, viewport);
        assert_eq!(max_scroll, 80);

        let target = next_hunk_offset(&starts, 10).expect("next hunk");
        let scroll = target.min(max_scroll);
        assert_eq!(target, 90);
        assert_eq!(scroll, 80);
        assert!((scroll..scroll + viewport).contains(&target));
        assert!((scroll..scroll + viewport).contains(&95));

        // Hunk navigation has no separate cursor: once the last page is
        // visible, another `]` targets the same hunk and leaves the page put.
        let repeated_target = next_hunk_offset(&starts, scroll).expect("visible next hunk");
        assert_eq!(repeated_target.min(max_scroll), scroll);
    }

    #[test]
    fn navigation_stops_at_edges_and_skips_known_no_ops() {
        let files = vec![
            FileModel {
                change: change(b"a"),
                no_op: false,
                load_error: None,
            },
            FileModel {
                change: change(b"b"),
                no_op: true,
                load_error: None,
            },
            FileModel {
                change: change(b"c"),
                no_op: false,
                load_error: None,
            },
        ];
        assert_eq!(next_visible(&files, 0, true), Some(2));
        assert_eq!(next_visible(&files, 2, true), None);
        assert_eq!(next_visible(&files, 2, false), Some(0));
        assert_eq!(next_visible(&files, 0, false), None);
    }

    #[test]
    fn adjacent_prefetch_prefers_forward_then_uncached_backward() {
        let files = vec![
            FileModel {
                change: change(b"a"),
                no_op: false,
                load_error: None,
            },
            FileModel {
                change: change(b"noop"),
                no_op: true,
                load_error: None,
            },
            FileModel {
                change: change(b"c"),
                no_op: false,
                load_error: None,
            },
            FileModel {
                change: change(b"d"),
                no_op: false,
                load_error: None,
            },
        ];

        assert_eq!(adjacent_prefetch_candidate(&files, 2, |_| false), Some(3));
        assert_eq!(
            adjacent_prefetch_candidate(&files, 2, |candidate| candidate == 3),
            Some(0)
        );
        assert_eq!(
            adjacent_prefetch_candidate(&files, 2, |candidate| {
                candidate == 0 || candidate == 3
            }),
            None
        );
    }

    #[test]
    fn app_navigation_methods_stop_at_edges() {
        let mut app = test_app();
        app.previous_file();
        assert_eq!(app.selected, 0);
        app.next_file();
        assert_eq!(app.selected, 1);
        app.next_file();
        assert_eq!(app.selected, 1);
        app.previous_file();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn body_reports_loading_binary_failure_and_clean_states() {
        let mut app = test_app();
        assert_eq!(app.body_lines(10)[0].spans[0].content, "Loading content…");

        app.activate_content(prepared(PreparedKind::Binary));
        assert_eq!(
            app.body_lines(10)[0].spans[0].content,
            "Binary files differ"
        );

        app.current_content = None;
        app.files[0].load_error = Some(load_error("a", "gone"));
        assert_eq!(
            app.body_lines(10)[0].spans[0].content,
            "Cannot read file: cannot read a: gone"
        );

        app.files.iter_mut().for_each(|file| file.no_op = true);
        assert_eq!(
            app.body_lines(10)[0].spans[0].content,
            "Working tree is clean."
        );
    }

    #[test]
    fn ctrl_c_and_q_are_quit_keys() {
        assert!(is_quit_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(is_quit_key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE
        )));
        assert!(!is_quit_key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE
        )));
    }

    #[test]
    fn title_includes_change_status() {
        assert_eq!(
            file_label(&change(b"new")),
            "A new [None → Some(Submodule)]"
        );
    }

    #[test]
    fn requested_view_selects_the_target_and_comparison_label_together() {
        assert_eq!(
            requested_target(RequestedView::Worktree, None),
            (DiffTarget::WorktreeVsHead, None)
        );

        let commit = Oid([7; 20]);
        assert_eq!(
            requested_target(
                RequestedView::Show(b"HEAD"),
                Some(CommitRevision {
                    commit,
                    has_parent: true,
                }),
            ),
            (
                DiffTarget::CommitVsParent { commit },
                Some("comparing HEAD^1..HEAD".to_owned()),
            )
        );
        assert_eq!(
            requested_target(
                RequestedView::Show(b"fixture-root"),
                Some(CommitRevision {
                    commit,
                    has_parent: false,
                }),
            ),
            (
                DiffTarget::CommitVsParent { commit },
                Some("comparing empty..fixture-root".to_owned()),
            )
        );
    }

    #[test]
    fn commit_comparison_describes_first_parent_and_root_ranges() {
        assert_eq!(
            commit_comparison_label(b"feature", true),
            "comparing feature^1..feature"
        );
        assert_eq!(
            commit_comparison_label(b"fixture-root", false),
            "comparing empty..fixture-root"
        );
    }

    #[test]
    fn commit_comparison_escapes_terminal_controls_and_invalid_bytes() {
        assert_eq!(
            commit_comparison_label(b"bad\x1b\xff", true),
            r"comparing bad\x1b\xff^1..bad\x1b\xff"
        );
    }

    #[test]
    fn title_includes_the_commit_comparison() {
        let mut app = test_app();
        app.comparison_label = Some("comparing HEAD^1..HEAD".to_owned());
        assert_eq!(
            app.title(),
            " tsuiku  comparing HEAD^1..HEAD  [1/2] A a [None → Some(Submodule)] "
        );
        app.files.iter_mut().for_each(|file| file.no_op = true);
        assert_eq!(app.title(), " tsuiku  comparing HEAD^1..HEAD  no changes ");
        assert_eq!(
            app.body_lines(10)[0].spans[0].content,
            "No changes in this comparison."
        );
    }

    #[test]
    fn structural_language_cannot_inject_terminal_controls() {
        assert_eq!(
            terminal_safe_label("Ru\u{1b}[31mst\u{85}"),
            r"Ru\u{1b}[31mst\u{85}"
        );
    }

    #[test]
    fn title_reports_no_changes_when_every_candidate_resolves_to_no_op() {
        let mut app = test_app();
        app.files.iter_mut().for_each(|file| file.no_op = true);
        assert_eq!(app.title(), " tsuiku  no changes ");
        assert_eq!(
            app.body_lines(10)[0].spans[0].content,
            "Working tree is clean."
        );
    }

    #[test]
    fn resize_clamps_scroll_to_the_taller_body() {
        let mut app = test_app();
        let source = (0..100).map(|line| format!("{line}\n")).collect::<String>();
        app.activate_content(prepared_text(&source, &source));
        wait_for_line_diff(&mut app);

        app.scroll = 80;
        let area = app.handle_resize(100, 63);

        assert_eq!(area, Size::new(100, 63));
        assert_eq!(body_height(area.height), 60);
        assert_eq!(app.scroll, 40);
    }

    #[test]
    fn bracket_keys_navigate_precomputed_hunks() {
        let mut app = test_app();
        app.activate_content(prepared_text(
            "a\nold one\nc\nd\nold two\nf\n",
            "a\nnew one\nc\nd\nnew two\nf\n",
        ));
        wait_for_line_diff(&mut app);
        assert_eq!(app.worker.hunk_starts(), &[1, 5]);

        assert!(app.handle_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE), 3));
        assert_eq!(app.scroll, 1);
        assert!(app.handle_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE), 3));
        assert_eq!(app.scroll, 5);
        assert!(app.handle_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE), 3));
        assert_eq!(app.scroll, 1);
    }

    #[test]
    fn bracket_key_never_scrolls_past_the_last_page() {
        let mut app = test_app();
        let old = (0..60)
            .map(|line| {
                if line == 58 {
                    "old\n".to_owned()
                } else {
                    format!("{line}\n")
                }
            })
            .collect::<String>();
        let new = old.replace("old\n", "new\n");
        app.activate_content(prepared_text(&old, &new));
        wait_for_line_diff(&mut app);
        assert_eq!(app.worker.hunk_starts(), &[58]);

        assert!(app.handle_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE), 5));
        assert_eq!(app.scroll, app.max_scroll(5));
        assert_eq!(app.scroll, 56);
    }

    fn refresh(changes: Vec<FileChange>, batch: EventBatch) -> WatchUpdate {
        WatchUpdate::Refresh {
            changes: crate::change::ChangeSet {
                target: DiffTarget::WorktreeVsHead,
                changes,
                warnings: Vec::new(),
            },
            batch,
        }
    }

    fn batch_of(paths: &[&[u8]]) -> EventBatch {
        EventBatch {
            paths: paths.iter().map(|path| GitPath::from_bytes(path)).collect(),
            ..EventBatch::default()
        }
    }

    #[test]
    fn unrelated_watch_refresh_carries_content_without_a_reload() {
        let mut app = test_app_with_prefetch();
        let content = prepared_text("old\n", "new\n");
        app.activate_content(Arc::clone(&content));
        app.scroll = 3;

        app.handle_watch_update(refresh(
            vec![change(b"a"), change(b"b"), change(b"c")],
            batch_of(&[b"unrelated.txt"]),
        ));

        assert_eq!(app.snapshot, SnapshotId(2));
        assert!(
            app.current_content
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &content)),
            "the displayed pair must be transferred, not reloaded"
        );
        assert_eq!(app.scroll, 3, "the reading position is kept");
        assert_eq!(
            app.resolver
                .as_ref()
                .expect("test coordinator")
                .queued_for_test()
                .map(|(_, priority)| priority),
            Some(LoadPriority::Prefetch),
            "only the adjacent prefetch may start, never a selected reload"
        );
    }

    #[test]
    fn watch_refresh_touching_the_selected_path_reloads_it() {
        let mut app = test_app_with_prefetch();
        let content = prepared_text("old\n", "new\n");
        app.activate_content(Arc::clone(&content));

        app.handle_watch_update(refresh(
            vec![change(b"a"), change(b"b"), change(b"c")],
            batch_of(&[b"a"]),
        ));

        assert!(app.current_content.is_none());
        assert_eq!(
            app.resolver
                .as_ref()
                .expect("test coordinator")
                .queued_for_test(),
            Some((0, LoadPriority::Selected)),
            "the selected pair must be re-read"
        );
    }

    #[test]
    fn lossy_watch_refresh_never_carries_content() {
        let mut app = test_app_with_prefetch();
        let content = prepared_text("old\n", "new\n");
        app.activate_content(Arc::clone(&content));

        let mut batch = batch_of(&[b"unrelated.txt"]);
        batch.overflow = true;
        app.handle_watch_update(refresh(
            vec![change(b"a"), change(b"b"), change(b"c")],
            batch,
        ));

        assert!(
            app.current_content.is_none(),
            "possible event loss voids the carry-over shortcut"
        );
        assert_eq!(
            app.resolver
                .as_ref()
                .expect("test coordinator")
                .queued_for_test(),
            Some((0, LoadPriority::Selected))
        );
    }

    #[test]
    fn degraded_watch_sets_the_notice_and_stops_polling() {
        let mut app = test_app();
        app.handle_watch_update(WatchUpdate::Degraded {
            reason: "inotify watch limit reached".to_owned(),
        });
        assert_eq!(
            app.watch_notice.as_deref(),
            Some("inotify watch limit reached")
        );
        assert!(app.watch.is_none());
        assert!(
            app.title()
                .contains("watch: off (inotify watch limit reached)"),
            "the stop reason must be displayed, not just the fact"
        );
    }

    #[test]
    fn degraded_watch_stays_visible_on_a_clean_tree() {
        let mut app = test_app();
        app.files.iter_mut().for_each(|file| file.no_op = true);
        app.handle_watch_update(WatchUpdate::Degraded {
            reason: "gone".to_owned(),
        });
        assert_eq!(app.title(), " tsuiku  no changes  watch: off (gone) ");
    }

    #[test]
    fn degraded_watch_reason_cannot_inject_terminal_controls() {
        let mut app = test_app();
        app.handle_watch_update(WatchUpdate::Degraded {
            reason: "bad\u{1b}[31mpath".to_owned(),
        });
        let title = app.title();
        assert!(title.contains(r"bad\u{1b}[31mpath"));
        assert!(!title.contains('\u{1b}'));
    }

    #[test]
    fn stale_generation_result_is_rejected_after_files_shift() {
        // A file inserted at the head means index 0 now names a different
        // file: the old generation's index-0 result must neither display
        // nor enter the (index-keyed) content cache.
        let mut app = test_app();
        let shifted = vec![change(b"inserted-at-head"), change(b"a"), change(b"b")];
        app.apply_snapshot(shifted);
        assert_eq!(app.snapshot, SnapshotId(2));
        assert_eq!(app.selected, 1, "the selection follows its path");

        app.handle_load_result(LoadResult {
            snapshot: SnapshotId(1),
            file_id: 0,
            result: Ok(prepared_value(PreparedKind::Binary)),
        });

        assert!(!app.content_cache.contains_key(&0));
        assert!(app.current_content.is_none());
    }

    #[test]
    fn deleting_the_selected_file_moves_the_selection_to_a_valid_entry() {
        let mut app = test_app();
        assert_eq!(app.files[0].change.display_path().as_bytes(), b"a");

        app.apply_snapshot(vec![change(b"b")]);

        assert_eq!(app.selected, 0);
        assert_eq!(
            app.files[app.selected].change.display_path().as_bytes(),
            b"b"
        );
        assert!(app.current_content.is_none());
    }

    #[test]
    fn stale_selected_result_arriving_after_the_switch_stays_pending() {
        // The same path stays selected across the switch, but the old
        // generation's bytes may be outdated: the display must stay in the
        // loading state until the new generation's result arrives.
        let mut app = test_app();
        app.apply_snapshot(vec![change(b"a"), change(b"b")]);
        assert_eq!(app.selected, 0);

        app.handle_load_result(LoadResult {
            snapshot: SnapshotId(1),
            file_id: 0,
            result: Ok(prepared_value(PreparedKind::Binary)),
        });

        assert!(app.current_content.is_none());
        assert!(!app.content_cache.contains_key(&0));
        assert_eq!(app.body_lines(10)[0].spans[0].content, "Loading content…");

        // The current generation's result still applies normally.
        app.handle_load_result(LoadResult {
            snapshot: SnapshotId(2),
            file_id: 0,
            result: Ok(prepared_value(PreparedKind::Binary)),
        });
        assert!(app.current_content.is_some());
    }

    #[test]
    fn snapshot_switch_drops_the_content_cache_and_follows_the_selected_path() {
        let mut app = test_app();
        app.selected = 1;
        app.content_cache
            .insert(0, prepared(PreparedKind::Binary), 64);

        app.apply_snapshot(vec![change(b"b"), change(b"a")]);

        assert_eq!(app.selected, 0, "path b moved to the head");
        assert!(
            !app.content_cache.contains_key(&0),
            "index-keyed entries are meaningless across generations"
        );
    }

    #[test]
    fn unselected_load_result_is_cached_without_replacing_visible_content() {
        let mut app = test_app();
        let selected = prepared(PreparedKind::Binary);
        app.activate_content(Arc::clone(&selected));

        app.handle_load_result(LoadResult {
            snapshot: SnapshotId(1),
            file_id: 1,
            result: Ok(prepared_value(PreparedKind::Binary)),
        });

        assert!(app.content_cache.contains_key(&1));
        assert!(
            app.current_content
                .as_ref()
                .is_some_and(|content| Arc::ptr_eq(content, &selected))
        );
    }

    #[test]
    fn prefetched_no_op_is_hidden_without_moving_the_selection() {
        let mut app = test_app();
        let selected = prepared(PreparedKind::Binary);
        app.activate_content(Arc::clone(&selected));

        app.handle_load_result(LoadResult {
            snapshot: SnapshotId(1),
            file_id: 1,
            result: Ok(prepared_value(PreparedKind::NoOp)),
        });

        assert!(app.files[1].no_op);
        assert_eq!(app.selected, 0);
        assert!(
            app.current_content
                .as_ref()
                .is_some_and(|content| Arc::ptr_eq(content, &selected))
        );
    }

    #[test]
    fn selected_load_completion_queues_adjacent_prefetch() {
        let mut app = test_app_with_prefetch();

        app.handle_load_result(LoadResult {
            snapshot: SnapshotId(1),
            file_id: 0,
            result: Ok(prepared_value(PreparedKind::Binary)),
        });

        assert_eq!(
            app.resolver
                .as_ref()
                .expect("test coordinator")
                .queued_for_test(),
            Some((1, LoadPriority::Prefetch))
        );
    }

    #[test]
    fn completed_prefetch_does_not_chain_past_visible_content() {
        let mut app = test_app_with_prefetch();
        app.handle_load_result(LoadResult {
            snapshot: SnapshotId(1),
            file_id: 0,
            result: Ok(prepared_value(PreparedKind::Binary)),
        });
        assert_eq!(
            app.resolver
                .as_ref()
                .expect("test coordinator")
                .take_queued_for_test(),
            Some((1, LoadPriority::Prefetch))
        );

        app.handle_load_result(LoadResult {
            snapshot: SnapshotId(1),
            file_id: 1,
            result: Ok(prepared_value(PreparedKind::Binary)),
        });

        assert_eq!(
            app.resolver
                .as_ref()
                .expect("test coordinator")
                .queued_for_test(),
            None
        );
    }

    #[test]
    fn no_op_prefetch_chains_to_the_next_visible_neighbor() {
        let mut app = test_app_with_prefetch();
        app.handle_load_result(LoadResult {
            snapshot: SnapshotId(1),
            file_id: 0,
            result: Ok(prepared_value(PreparedKind::Binary)),
        });
        assert_eq!(
            app.resolver
                .as_ref()
                .expect("test coordinator")
                .take_queued_for_test(),
            Some((1, LoadPriority::Prefetch))
        );

        app.handle_load_result(LoadResult {
            snapshot: SnapshotId(1),
            file_id: 1,
            result: Ok(prepared_value(PreparedKind::NoOp)),
        });

        assert!(app.files[1].no_op);
        assert_eq!(
            app.resolver
                .as_ref()
                .expect("test coordinator")
                .queued_for_test(),
            Some((2, LoadPriority::Prefetch))
        );
    }

    #[test]
    fn selected_no_op_moves_forward_to_the_next_visible_file() {
        let mut app = test_app();

        app.handle_load_result(LoadResult {
            snapshot: SnapshotId(1),
            file_id: 0,
            result: Ok(prepared_value(PreparedKind::NoOp)),
        });

        assert!(app.files[0].no_op);
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn selected_no_op_moves_backward_when_no_visible_file_follows() {
        let mut app = test_app();
        app.selected = 1;

        app.handle_load_result(LoadResult {
            snapshot: SnapshotId(1),
            file_id: 1,
            result: Ok(prepared_value(PreparedKind::NoOp)),
        });

        assert!(app.files[1].no_op);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn load_error_keeps_its_type_and_is_cleared_when_reselected() {
        let mut app = test_app();

        app.handle_load_result(LoadResult {
            snapshot: SnapshotId(1),
            file_id: 0,
            result: Err(load_error("a", "gone")),
        });

        assert!(matches!(
            app.files[0].load_error.as_ref(),
            Some(ResolveError::Io { path, .. }) if path == "a"
        ));
        app.request_selected();
        assert!(app.files[0].load_error.is_none());
    }

    #[test]
    fn first_content_kind_covers_non_text_outcomes() {
        let mut app = test_app();
        assert_eq!(app.visible_content_kind(), None);

        app.activate_content(prepared(PreparedKind::Binary));
        assert_eq!(app.visible_content_kind(), Some(FirstContentKind::Binary));

        app.files.iter_mut().for_each(|file| file.no_op = true);
        assert_eq!(app.visible_content_kind(), Some(FirstContentKind::Clean));
    }
}
