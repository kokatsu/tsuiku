//! Interactive terminal viewer for worktree changes.
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
use ratatui::layout::{Constraint, Direction, Layout, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::asyncstate::{
    AsyncState, LINE_MODEL_VERSION, LineDiffCacheKey, LineDiffEngineId, LineDiffUnavailable,
    StructuralSkip,
};
use crate::cache::WeightedLru;
use crate::change::{ChangeDiscoverer, ChangeQuery, ChangeStatus, DiffTarget, FileChange};
use crate::discover::GixDiscoverer;
use crate::loader::{ContentLoadCoordinator, LoadResult, PreparedContent, PreparedKind};
use crate::resolve::{GixResolver, ResolveError};
use crate::structural::normalize::DifftStatus;
use crate::structural::tempfiles::LanguagePathHint;
use crate::structural_worker::StructuralDiffCoordinator;
use crate::view::build_unified_lines_with_overlay;
use crate::worker::LineDiffCoordinator;

// The layout reserves two title rows and one footer row. Scrolling and
// rendering both subtract this value so they agree on the body height.
const CHROME_HEIGHT: u16 = 3;
// A single entry may exceed its budget so even one unusually large selected
// file remains viewable; inserting it evicts older entries.
const CONTENT_CACHE_BYTES: usize = 32 * 1024 * 1024;
const LINE_DIFF_CACHE_BYTES: usize = 16 * 1024 * 1024;
const STRUCTURAL_DIFF_CACHE_BYTES: usize = 16 * 1024 * 1024;
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

/// Mutable state of one interactive worktree-viewer session.
///
/// `App` owns the navigation state, background coordinators, and caches. It
/// does not own terminal restoration; [`Self::run_path`] uses a separate guard
/// so raw mode is also restored after errors and panics.
pub struct App {
    files: Vec<FileModel>,
    selected: usize,
    scroll: usize,
    resolver: Option<ContentLoadCoordinator>,
    content_cache: WeightedLru<usize, Arc<PreparedContent>>,
    current_content: Option<Arc<PreparedContent>>,
    worker: LineDiffCoordinator,
    structural_worker: StructuralDiffCoordinator,
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

    fn load(path: &Path, started_at: Instant) -> Result<Self, AppError> {
        let discoverer = GixDiscoverer::open(path).map_err(AppError::Discover)?;
        let set = discoverer
            .discover(&ChangeQuery::new(DiffTarget::WorktreeVsHead))
            .map_err(AppError::Discover)?;
        let (repo, paths) = discoverer.into_parts();
        let paths = paths.ok_or(AppError::Discover(crate::change::DiscoverError::NoWorktree))?;
        let resolver = GixResolver::new(repo, paths);
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
            selected: 0,
            scroll: 0,
            resolver: Some(ContentLoadCoordinator::new(resolver)),
            content_cache: WeightedLru::new(CONTENT_CACHE_BYTES),
            current_content: None,
            worker: LineDiffCoordinator::new(LINE_DIFF_CACHE_BYTES),
            structural_worker: StructuralDiffCoordinator::new(STRUCTURAL_DIFF_CACHE_BYTES),
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
        let Some(file) = self.files.get_mut(self.selected) else {
            return;
        };
        if file.no_op {
            return;
        }
        file.load_error = None;
        if let Some(content) = self.content_cache.get_cloned(&self.selected) {
            self.activate_content(content);
            return;
        }
        if let Some(resolver) = &self.resolver {
            resolver.request(self.selected, file.change.clone());
        }
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
                self.structural_worker.request(
                    content.pair,
                    change
                        .old_path
                        .as_ref()
                        .map(LanguagePathHint::from_git_path)
                        .unwrap_or_else(LanguagePathHint::none),
                    change
                        .new_path
                        .as_ref()
                        .map(LanguagePathHint::from_git_path)
                        .unwrap_or_else(LanguagePathHint::none),
                    Arc::clone(content.old.as_ref().expect("text old side")),
                    Arc::clone(content.new.as_ref().expect("text new side")),
                );
            }
        }
        self.current_content = Some(content);
    }

    fn handle_load_result(&mut self, result: LoadResult) {
        match result.result {
            Ok(content) if content.kind == PreparedKind::NoOp => {
                // Discovery can conservatively report a candidate whose
                // resolved sides are identical. Remove it from navigation.
                if let Some(file) = self.files.get_mut(result.file_id) {
                    file.no_op = true;
                }
                if result.file_id == self.selected {
                    if let Some(next) = next_visible(&self.files, self.selected, true)
                        .or_else(|| next_visible(&self.files, self.selected, false))
                    {
                        self.selected = next;
                        self.request_selected();
                    } else {
                        self.current_content = None;
                        self.worker.reset();
                        self.structural_worker.reset();
                    }
                }
            }
            Ok(content) => {
                let content = Arc::new(content);
                let weight = content.estimated_bytes();
                self.content_cache
                    .insert(result.file_id, Arc::clone(&content), weight);
                if result.file_id == self.selected {
                    self.activate_content(content);
                }
            }
            Err(error) => {
                if let Some(file) = self.files.get_mut(result.file_id) {
                    file.load_error = Some(error);
                }
            }
        }
    }

    fn poll_workers(&mut self) -> bool {
        // Line-diff results perform their own cache-key check before becoming
        // visible. Content results use file_id for the equivalent check below.
        let mut dirty = self.worker.poll();
        dirty |= self.structural_worker.poll();
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
        let mut app = Self::load(path, started_at)?;
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
                let body = self.body_lines(body_height(terminal_area.height));
                let content_kind = self.visible_content_kind();
                record_metric(
                    self.metrics_enabled,
                    &self.frame_build_micros,
                    elapsed_micros(build_started),
                );
                let draw_started = Instant::now();
                terminal.draw(|frame| self.draw(frame, body))?;
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

    fn draw<'a>(&'a self, frame: &mut ratatui::Frame<'_>, body: Vec<Line<'a>>) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(frame.area());

        let title = self.title();
        frame.render_widget(
            Paragraph::new(title)
                .style(Style::default().add_modifier(Modifier::BOLD))
                .block(Block::default().borders(Borders::BOTTOM)),
            chunks[0],
        );
        frame.render_widget(Paragraph::new(body), chunks[1]);
        frame.render_widget(
            Paragraph::new(" j/k scroll  n/p file  PgUp/PgDn  q/Ctrl-C quit ")
                .style(Style::default().fg(Color::DarkGray)),
            chunks[2],
        );
    }

    fn body_lines(&self, height: usize) -> Vec<Line<'_>> {
        if self.visible_file_count() == 0 {
            return vec![Line::from("Working tree is clean.")];
        }
        let Some(file) = self.files.get(self.selected) else {
            return vec![Line::from("Working tree is clean.")];
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
                match self.structural_worker.state() {
                    AsyncState::Ready(overlay) => Some(overlay.as_ref()),
                    _ => None,
                },
                self.scroll,
                height,
            ),
        }
    }

    fn visible_file_count(&self) -> usize {
        self.files.iter().filter(|file| !file.no_op).count()
    }

    fn title(&self) -> String {
        let visible_count = self.visible_file_count();
        if visible_count == 0 {
            return " tsuiku  no changes ".to_owned();
        }
        let Some(file) = self.files.get(self.selected) else {
            return " tsuiku  no changes ".to_owned();
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
                    " tsuiku  [{ordinal}/{visible_count}] {}  structural: {} {}/{} ",
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
            " tsuiku  [{ordinal}/{visible_count}] {}{structural} ",
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

fn next_visible(files: &[FileModel], selected: usize, forward: bool) -> Option<usize> {
    if forward {
        ((selected + 1)..files.len()).find(|&index| !files[index].no_op)
    } else {
        (0..selected).rev().find(|&index| !files[index].no_op)
    }
}

fn body_height(terminal_height: u16) -> usize {
    terminal_height.saturating_sub(CHROME_HEIGHT) as usize
}

fn max_scroll_for_rows(rows: usize, viewport_height: usize) -> usize {
    rows.saturating_sub(viewport_height)
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
    use crate::path::GitPath;
    use crate::text::{ClassifiedContent, classify};

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
            selected: 0,
            scroll: 0,
            resolver: None,
            content_cache: WeightedLru::new(1024 * 1024),
            current_content: None,
            worker: LineDiffCoordinator::new(1024 * 1024),
            structural_worker: StructuralDiffCoordinator::new(1024 * 1024),
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

    #[test]
    fn viewport_height_and_scroll_use_the_same_chrome_height() {
        assert_eq!(body_height(24), 21);
        assert_eq!(max_scroll_for_rows(100, body_height(24)), 79);
        assert_eq!(max_scroll_for_rows(10, body_height(24)), 0);
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
    fn unselected_load_result_is_cached_without_replacing_visible_content() {
        let mut app = test_app();
        let selected = prepared(PreparedKind::Binary);
        app.activate_content(Arc::clone(&selected));

        app.handle_load_result(LoadResult {
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
    fn selected_no_op_moves_forward_to_the_next_visible_file() {
        let mut app = test_app();

        app.handle_load_result(LoadResult {
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
