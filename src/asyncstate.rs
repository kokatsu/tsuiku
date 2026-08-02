//! Shared states and cache keys for background diff computations.
//!
//! Every request and result carries its cache key; a result is applied only
//! when `result.cache_key` equals the key currently derived from the content
//! pair on screen. Results for content that is no longer selected are still
//! stored in the cache, so navigating back can reuse them, but are never
//! displayed for the current selection.

use std::sync::Arc;

use crate::ids::{ContentId, ContentPairId};
use crate::linediff::DiffRow;
use crate::structural::normalize::StructuralOverlay;
use crate::structural::tempfiles::LanguagePathHint;
use crate::syntax::{SyntaxSpans, ThemeId};

/// Monotonic id for one in-flight worker request.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct RequestId(pub u64);

/// Lifecycle of an asynchronously computed value.
#[derive(Clone, Debug)]
pub enum AsyncState<T, Skip, Err> {
    NotRequested,
    Pending {
        request_id: RequestId,
    },
    Ready(T),
    /// Computed on purpose not at all: a known, displayable reason.
    Skipped(Skip),
    Failed(Err),
}

pub type LineDiffState = AsyncState<Arc<[DiffRow]>, LineDiffUnavailable, LineDiffError>;
pub type StructuralDiffState = AsyncState<Arc<StructuralOverlay>, StructuralSkip, StructuralError>;
pub type SyntaxHighlightState = AsyncState<Arc<SyntaxSpans>, SyntaxSkip, SyntaxError>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineDiffUnavailable {
    Binary,
    SizeLimited { bytes: u64, limit: u64 },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LineDiffError {
    WorkerGone,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StructuralSkip {
    UnsupportedLanguage,
    SizeLimited,
    ToolUnavailable,
    IncompatibleVersion,
    /// One side is absent (add or delete). Difftastic reports such a pair as
    /// created/deleted and emits no change spans, so nothing is computed.
    OneSided,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum StructuralError {
    /// The difft binary does not exist. Distinct from `Io` so callers can
    /// treat a missing tool as a capability gap rather than a failure.
    ToolNotFound,
    TimedOut,
    /// The owner asked the run to stop (shutdown). Never displayed: the
    /// coordinator that would show it is the one being dropped.
    Cancelled,
    ProcessFailed {
        exit_code: Option<i32>,
    },
    OutputTooLarge,
    InvalidJson,
    InvalidSchema,
    Io,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SyntaxSkip {
    UnsupportedLanguage,
    SizeLimited,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SyntaxError {
    /// syntect failed to parse or apply scopes for this content.
    HighlightFailed,
    WorkerGone,
}

/// Bump when the line splitting / line index contract changes.
pub const LINE_MODEL_VERSION: u32 = 1;

/// Bump when the syntect span extraction rules change.
pub const HIGHLIGHTER_VERSION: u32 = 1;

/// Bump when the difft JSON normalization rules change.
pub const NORMALIZER_VERSION: u32 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LineDiffEngineId {
    Imara,
    Similar,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct LineDiffCacheKey {
    pub pair: ContentPairId,
    pub engine: LineDiffEngineId,
    pub options_fingerprint: u64,
    pub line_model_version: u32,
}

/// Cache key for one highlighted side. Keyed by `ContentId`, not the pair:
/// old and new hit the cache independently.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct SyntaxHighlightCacheKey {
    pub content: ContentId,
    pub language_hint: LanguagePathHint,
    pub theme_id: ThemeId,
    pub highlighter_version: u32,
    pub options_fingerprint: u64,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct StructuralDiffCacheKey {
    pub pair: ContentPairId,
    pub old_path_hint: LanguagePathHint,
    pub new_path_hint: LanguagePathHint,
    pub difft_version: String,
    pub normalizer_version: u32,
    pub options_fingerprint: u64,
}
