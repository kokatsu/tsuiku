//! Structural diff via difftastic, run as an isolated subprocess.
//!
//! Pipeline: write both sides to temp files with a language hint in the file
//! name (`tempfiles`), run `difft --display json` (`runner`), deserialize the
//! raw JSON (`json`), then validate and normalize every span against our own
//! line view (`normalize`). Only spans that survive validation are ever
//! rendered; the raw difft output is never trusted for layout.

pub mod json;
pub mod normalize;
pub mod runner;
pub mod tempfiles;
