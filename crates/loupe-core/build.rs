// `include_dir!` embeds the `src/starlark` tree at compile time but does not
// register the files as build inputs, so editing a `.bzl` does not trigger a
// rebuild and the binary keeps a stale embed. Walk the tree and emit a
// rerun-if-changed line per file (a directory-level line only tracks the
// directory's own mtime, which content edits don't bump).
fn main() {
    emit_rerun("src/starlark");
}

fn emit_rerun(dir: &str) {
    println!("cargo:rerun-if-changed={dir}");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            emit_rerun(path.to_str().expect("utf-8 path"));
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
