//! Shell out to `bazel cquery --output=starlark`.

use std::process::Command;

use anyhow::{Context, Result, anyhow};

use crate::Fixture;

/// Env var for extra Bazel startup args (e.g. JVM trust-store overrides
/// for restricted environments). Whitespace-split, prepended before the
/// `cquery` subcommand.
pub const STARTUP_ARGS_ENV: &str = "LOUPE_BAZEL_STARTUP_ARGS";

pub fn cquery(fx: &Fixture, target: &str, expr: Option<&str>) -> Result<String> {
    let mut cmd = Command::new("bazel");
    cmd.current_dir(fx.root());
    if let Ok(extra) = std::env::var(STARTUP_ARGS_ENV) {
        for arg in extra.split_whitespace() {
            cmd.arg(arg);
        }
    }
    cmd.arg("cquery").arg(target).arg("--output=starlark");
    if let Some(e) = expr {
        cmd.arg(format!("--starlark:expr={e}"));
    }
    let out = cmd
        .output()
        .with_context(|| "spawning `bazel` (is it on PATH?)")?;
    if !out.status.success() {
        return Err(anyhow!(
            "bazel cquery failed (exit {}):\n--- stderr ---\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}
