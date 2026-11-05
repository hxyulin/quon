use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Source location for a diagnostic. Start and end positions are tracked
/// so renderers can underline a token range; when the span is a single
/// point, `end_line == line` and `end_col == col`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSpan {
    pub file: PathBuf,
    pub line: u32,
    pub col: u32,
    pub end_line: u32,
    pub end_col: u32,
}

impl SourceSpan {
    /// A zero-length span at `(line, col)`.
    pub fn point(file: impl Into<PathBuf>, line: u32, col: u32) -> Self {
        let file = file.into();
        Self {
            file,
            line,
            col,
            end_line: line,
            end_col: col,
        }
    }

    /// A span from `(start_line, start_col)` to `(end_line, end_col)`.
    pub fn range(file: impl Into<PathBuf>, start: (u32, u32), end: (u32, u32)) -> Self {
        Self {
            file: file.into(),
            line: start.0,
            col: start.1,
            end_line: end.0,
            end_col: end.1,
        }
    }
}

/// Renders as `file:line:col`. The end position is tracked for range
/// underlining but intentionally not included in the default display form
/// at this stage.
impl std::fmt::Display for SourceSpan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.file.display(), self.line, self.col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_format() {
        let s = SourceSpan::point("foo.rs", 12, 5);
        assert_eq!(s.to_string(), "foo.rs:12:5");
    }
}
