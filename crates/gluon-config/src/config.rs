use gluon_model::RawBuildModel;
use crate::error::Result;

pub struct Config {
    pub(crate) raw: RawBuildModel,
}

impl Config {
    pub fn new() -> Self {
        Self { raw: RawBuildModel::default() }
    }

    /// Serialise the underlying `RawBuildModel` as pretty-printed JSON to
    /// stdout. Called from the user's `main()`.
    pub fn emit(&self) -> Result<()> {
        serde_json::to_writer_pretty(std::io::stdout().lock(), &self.raw)
            .map_err(|e| crate::error::ConfigError::new(format!("emit: {e}")))?;
        println!();
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_config_round_trips_to_empty_raw() {
        let cfg = Config::new();
        let json = serde_json::to_string(&cfg.raw).expect("serde");
        let de: RawBuildModel = serde_json::from_str(&json).expect("de");
        assert!(de.model.project.is_none());
        assert!(de.kconfig_includes.is_empty());
    }
}
