//! Chainable builder for the singleton project entry.

use crate::builders::BuilderState;
use crate::config::Config;

/// Chainable builder returned by [`Config::project`].
///
/// `project` is a singleton on the [`Config`]; the [`BuilderState`]
/// type parameter is `()` because there is no arena slot to point at.
/// Subsequent calls to `Config::project` overwrite the previous
/// definition (last-call-wins) — this matches the engine's behaviour
/// and the long-standing test-fixture idiom of overwriting in setup
/// helpers.
pub struct ProjectBuilder<'cfg> {
    pub(crate) cfg: &'cfg mut Config,
    pub(crate) state: BuilderState<()>,
}

impl<'cfg> ProjectBuilder<'cfg> {
    /// Set the profile the CLI picks when the user does not pass
    /// `-p/--profile`. The referenced profile must exist by the time
    /// the model is interned; until then the value is recorded as a
    /// raw string and the validator catches dangling references.
    pub fn default_profile(self, name: &str) -> Self {
        if !self.state.is_duplicate() {
            if let Some(p) = self.cfg.raw.model.project.as_mut() {
                p.default_profile = Some(name.into());
            }
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use crate::Config;

    #[test]
    fn project_default_profile_records_string() {
        let mut cfg = Config::new();
        cfg.project("demo", "0.1.0").default_profile("dev");
        let p = cfg.raw.model.project.as_ref().unwrap();
        assert_eq!(p.default_profile.as_deref(), Some("dev"));
    }
}
