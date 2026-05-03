//! Structural diff between Bazel and Loupe cquery output.
//!
//! Phase 1: line-level comparison with a useful "Loupe produced
//! nothing" report. Future phases will need real structural parsing
//! (provider-aware, ordering-aware) — at that point this module grows.

use std::fmt;

#[derive(Debug, Clone)]
pub struct Report {
    pub bazel: String,
    pub loupe: String,
}

impl Report {
    pub fn is_divergent(&self) -> bool {
        self.bazel.trim() != self.loupe.trim()
    }
}

pub fn compare(bazel: &str, loupe: &str) -> Report {
    Report {
        bazel: bazel.to_string(),
        loupe: loupe.to_string(),
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b: Vec<&str> = self.bazel.lines().collect();
        let l: Vec<&str> = self.loupe.lines().collect();
        writeln!(
            f,
            "comparison report ({} bazel line(s), {} loupe line(s))",
            b.len(),
            l.len()
        )?;
        if !self.is_divergent() {
            writeln!(f, "  identical")?;
            return Ok(());
        }
        if l.is_empty() {
            writeln!(f, "  Loupe produced nothing; Bazel produced:")?;
            for line in &b {
                writeln!(f, "  | {line}")?;
            }
            return Ok(());
        }
        let bazel_only: Vec<&&str> = b.iter().filter(|x| !l.contains(x)).collect();
        let loupe_only: Vec<&&str> = l.iter().filter(|x| !b.contains(x)).collect();
        if !bazel_only.is_empty() {
            writeln!(f, "  bazel-only lines:")?;
            for line in bazel_only {
                writeln!(f, "  - {line}")?;
            }
        }
        if !loupe_only.is_empty() {
            writeln!(f, "  loupe-only lines:")?;
            for line in loupe_only {
                writeln!(f, "  + {line}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_outputs_are_not_divergent() {
        let r = compare("foo\nbar", "foo\nbar");
        assert!(!r.is_divergent());
    }

    #[test]
    fn empty_loupe_is_divergent_from_nonempty_bazel() {
        let r = compare("@@//:world", "");
        assert!(r.is_divergent());
        let rendered = format!("{r}");
        assert!(rendered.contains("Loupe produced nothing"));
        assert!(rendered.contains("@@//:world"));
    }

    #[test]
    fn line_diffs_show_both_sides() {
        let r = compare("a\nb\nc", "a\nx\nc");
        let rendered = format!("{r}");
        assert!(rendered.contains("- b"));
        assert!(rendered.contains("+ x"));
    }
}
