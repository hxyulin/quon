//! Chainable builders shared by `Config::profile`, `Config::group`, etc.

use gluon_model::SourceSpan;

/// Per-builder state. Every `XBuilder<'cfg>` carries one of these and
/// short-circuits chained methods when it's `Duplicate`. This collapses
/// the per-method `if is_duplicate { return ... }` boilerplate that the
/// engine's `builder_method!` macro spread across every chained call.
#[derive(Debug)]
pub enum BuilderState<S> {
    /// First-time creation. `state` is whatever sibling data the
    /// builder needs (typically a payload like a Handle or a name).
    Valid {
        state: S,
        name: String,
        span: Option<SourceSpan>,
    },
    /// Duplicate name. Methods on the builder must read this and return
    /// without touching the parent `RawBuildModel`.
    Duplicate {
        name: String,
        span: Option<SourceSpan>,
    },
}

impl<S> BuilderState<S> {
    pub fn is_duplicate(&self) -> bool {
        matches!(self, BuilderState::Duplicate { .. })
    }

    pub fn name(&self) -> &str {
        match self {
            BuilderState::Valid { name, .. } | BuilderState::Duplicate { name, .. } => name,
        }
    }

    /// Apply `f` to the state if `Valid`; no-op when `Duplicate`. The
    /// closure is the body of every chained method.
    pub fn with<F: FnOnce(&mut S)>(&mut self, f: F) {
        if let BuilderState::Valid { state, .. } = self {
            f(state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_state_invokes_closure() {
        let mut s: BuilderState<u32> = BuilderState::Valid {
            state: 0, name: "x".into(), span: None,
        };
        s.with(|n| *n = 42);
        match s {
            BuilderState::Valid { state, .. } => assert_eq!(state, 42),
            _ => panic!("must be Valid"),
        }
    }

    #[test]
    fn duplicate_state_skips_closure() {
        let mut s: BuilderState<u32> = BuilderState::Duplicate {
            name: "x".into(), span: None,
        };
        let mut called = false;
        s.with(|_| called = true);
        assert!(!called, "closure must not run for Duplicate");
    }

    #[test]
    fn name_returns_inner_name_in_both_states() {
        let v: BuilderState<()> = BuilderState::Valid { state: (), name: "foo".into(), span: None };
        let d: BuilderState<()> = BuilderState::Duplicate { name: "bar".into(), span: None };
        assert_eq!(v.name(), "foo");
        assert_eq!(d.name(), "bar");
    }
}
