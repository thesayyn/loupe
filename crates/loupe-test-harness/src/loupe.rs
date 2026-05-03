//! Loupe-side query. Stub until the engine exists (Phase 5).

use anyhow::Result;

use crate::Fixture;

// STUB: engine not implemented yet. Phase 1 expects this to return
// nothing so the harness reports "Loupe produced nothing; Bazel
// produced X". Phase 5 wires the real `Engine` through here.
pub fn cquery(_fx: &Fixture, _target: &str, _expr: Option<&str>) -> Result<String> {
    Ok(String::new())
}
