//! `quon explain`: how Quon would build the project, without building
//! it.

use super::CmdContext;
use anyhow::Result;
use quon_core::config::RunSelection;

/// `quon explain copies`: print the crates that building every entry
/// compiles more than once, and why. See [`quon_core::copies`].
pub fn copies(ctxs: &[CmdContext], selections: &[RunSelection]) -> Result<()> {
    let [first, ..] = ctxs else {
        return Ok(());
    };
    let names: Vec<&str> = selections
        .iter()
        .map(|s| match s {
            RunSelection::Named(name) => name.as_str(),
            _ => "the project",
        })
        .collect();
    let entries: Vec<_> = ctxs
        .iter()
        .zip(&names)
        .map(|(c, name)| (*name, &c.model, &c.resolved))
        .collect();
    let groups = quon_core::find_copies(&first.ctx, &entries)?;
    if groups.is_empty() {
        println!(
            "every crate compiles once across {} run {}",
            ctxs.len(),
            if ctxs.len() == 1 { "entry" } else { "entries" }
        );
    } else {
        print!("{}", quon_core::copies::render(&groups));
    }
    Ok(())
}
