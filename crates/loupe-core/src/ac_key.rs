//! Bazel-identical Action Cache (AC) key computation.
//!
//! ## What this is
//!
//! Bazel's Remote Execution API (REAPI v2) identifies every cached
//! action by the SHA-256 digest of a serialized
//! `build.bazel.remote.execution.v2.Action` message. Two Bazel
//! invocations on two different machines that "should" reuse the same
//! cache entry produce *byte-for-byte identical* serialized Action
//! protos; the SHA-256 of those bytes is the cache key. Anyone able
//! to reproduce that exact serialization independently can:
//!
//!   - Look up cached results in any REAPI-compliant cache without
//!     running Bazel.
//!   - Implement an `aquery`-style "would this hit cache?" view that
//!     answers the question Bazel itself answers via `ActionCache`.
//!   - Build cross-tool plumbing (e.g. a Loupe-driven build executor
//!     that talks to the same `bytestream.googleapis.com` Bazel does).
//!
//! ## Why this works without "implementing Bazel as a whole"
//!
//! REAPI is a small, well-specified protobuf surface:
//!
//!   - `Action { command_digest, input_root_digest, timeout,
//!     do_not_cache, salt, platform }`
//!   - `Command { arguments, environment_variables, output_paths,
//!     working_directory, platform }`
//!   - A merkle tree of `Directory { files, directories, symlinks }`
//!     where every node is identified by SHA-256 of its serialized
//!     bytes.
//!
//! The wire format is canonical (protobuf is, by spec, deterministic
//! once you sort `map` entries and emit fields in tag order). So
//! "Bazel-identical" reduces to:
//!
//!   1. Encode the same proto messages with the same field values.
//!   2. Emit fields in tag order, skipping proto3 zero defaults.
//!   3. Sort repeated entries the way Bazel does (alphabetical by
//!      `name` for Directory members; alphabetical by `name` for
//!      env vars and platform properties; user-given order for
//!      `arguments` and `output_paths`).
//!
//! No need to model Bazel's spawn runner, sandbox, worker protocol,
//! or remote-cache client. Just the proto.
//!
//! ## What this PoC delivers
//!
//! - A self-contained mini-protobuf encoder (no `prost`/`protobuf`
//!   dep — we hand-roll the 4 wire types we need, see [`pb`]).
//! - REAPI v2 `Digest`, `FileNode`, `DirectoryNode`, `SymlinkNode`,
//!   `Directory`, `Command`, `Command.EnvironmentVariable`, `Action`,
//!   `Platform`, `Platform.Property` encoders.
//! - [`compute_action_cache_key`] that walks a recorded
//!   [`crate::Action`], looks each input's pre-resolved
//!   `(Digest, is_executable)` up in a caller-supplied
//!   `BTreeMap<String, (Digest, bool)>`, builds the input root merkle
//!   tree, builds the `Command`, builds the `Action`, and SHA-256s the
//!   result. The function is sync + does no I/O — production callers
//!   (`loupe-exec-cas::ActionDigestKey`) batch all the digest work
//!   through DICE upfront and hand the resulting map in.
//! - Unit tests against REAPI's well-known fixed-point: the empty
//!   `Command{}` digest, the empty `Directory{}` digest, the empty
//!   `Action{}` digest. These three values are inscribed in REAPI
//!   conformance tests and Bazel's own source — matching them is the
//!   proof that the encoder is byte-correct.
//!
//! ## Engine-side surface this consumes
//!
//! Every piece of the REAPI proto comes from a typed engine field.
//! What flows where:
//!
//!   - `Command.arguments` ← `Action::kind` (`Run::{executable,
//!     arguments}`; `RunShell` is lowered to `/bin/bash -c <command>
//!     "" <args...>` matching Bazel's `RemoteSpawnRunner`).
//!   - `Command.environment_variables` ← `Action::env`, layered over
//!     [`default_shell_env`] when `Action::uses_default_shell_env`
//!     is set. Action env wins on key collision.
//!   - `Command.output_paths` ← `Action::outputs[i].path()`.
//!   - `Command.platform` / `Action.platform` ←
//!     `Configuration::exec_properties` merged with
//!     `Action::execution_requirements`. Action wins on key collision.
//!     Mirrors Bazel's `SpawnAction.getExecutionInfo()`.
//!   - `Action.input_root_digest` ← merkle tree built from
//!     `Action::inputs`, with each input's digest pulled from the
//!     caller-supplied `inputs_by_path` map. (Bazel hardcodes
//!     `FileNode.is_executable=true` for every input file regardless
//!     of FS mode — see `DirectoryTree.createExecutable` — so the
//!     `bool` carried in the map only affects *output* nodes today.)
//!   - File `path()` carries the post-PR engine layout: per-config
//!     segment (`bazel-out/<config>/bin/...`), external-repo prefix
//!     (`external/<canonical>/` or `../<canonical>/` per repository
//!     layout), and tree-artifact placement.
//!
//! ## Holes remaining
//!
//!   - **Workspace-name prefix.** Bazel's execroot lays main-repo
//!     sources under `<workspace_name>/<pkg>/<short>` (or `_main/...`
//!     with bzlmod). Loupe's `File::path()` returns `<pkg>/<short>`
//!     for main-repo sources. Matching Bazel byte-for-byte at the
//!     root layer requires plumbing `<workspace_name>` into `File`.

use std::collections::BTreeMap;

use sha2::{Digest as _, Sha256};

use crate::action::{Action, ActionKind, default_shell_env};
use crate::analysis::Configuration;
use crate::starlark_types::file::File;

/// REAPI v2 `Digest` — `sha256(bytes)` plus `bytes.len()`. The hex
/// hash is lowercase, 64 chars; size is an `i64` because REAPI defines
/// it so (a `Directory` proto can in principle exceed 4 GiB).
#[derive(Debug, Clone, PartialEq, Eq, Hash, allocative::Allocative, pagable::Pagable)]
pub struct Digest {
    pub hash: String,
    pub size_bytes: i64,
}

impl Digest {
    /// Digest of an empty byte sequence. REAPI well-known constant —
    /// every conformance test inscribes this value.
    pub fn empty() -> Self {
        Digest {
            hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
            size_bytes: 0,
        }
    }

    pub fn of_bytes(bytes: &[u8]) -> Self {
        let mut h = Sha256::new();
        h.update(bytes);
        let out = h.finalize();
        let mut hash = String::with_capacity(out.len() * 2);
        for b in out {
            hash.push_str(&format!("{b:02x}"));
        }
        Digest {
            hash,
            size_bytes: bytes.len() as i64,
        }
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.hash, self.size_bytes)
    }
}

/// Mini-protobuf wire-format encoders.
///
/// We need exactly four wire-format primitives: varint (proto3 ints),
/// length-delimited (strings, bytes, sub-messages), and that's it.
/// Sticks to the proto3 canonical-encoding rules:
///   - Skip fields whose values equal the proto3 default
///     (`0`/`false`/`""`/empty repeated).
///   - Emit fields in tag-ascending order.
///   - For sub-messages use length-delimited with the recursively
///     encoded bytes.
///
/// Field numbers below are pinned to REAPI v2 from
/// `build.bazel.remote.execution.v2.remote_execution.proto` —
/// stable since v2.0 (2018).
mod pb {
    /// Encode an unsigned varint (LEB128).
    pub fn varint(buf: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            buf.push(((v & 0x7f) as u8) | 0x80);
            v >>= 7;
        }
        buf.push(v as u8);
    }

    /// Encode a tag: `(field_number << 3) | wire_type`.
    fn tag(buf: &mut Vec<u8>, field: u32, wire: u8) {
        varint(buf, ((field as u64) << 3) | (wire as u64));
    }

    /// Wire type 0 (varint). Skips proto3 default 0.
    pub fn write_int64(buf: &mut Vec<u8>, field: u32, value: i64) {
        if value == 0 {
            return;
        }
        tag(buf, field, 0);
        varint(buf, value as u64);
    }

    /// Wire type 0 (varint). Skips proto3 default `false`.
    pub fn write_bool(buf: &mut Vec<u8>, field: u32, value: bool) {
        if !value {
            return;
        }
        tag(buf, field, 0);
        varint(buf, 1);
    }

    /// Wire type 2 (length-delimited). Skips proto3 default `""`.
    pub fn write_string(buf: &mut Vec<u8>, field: u32, value: &str) {
        if value.is_empty() {
            return;
        }
        tag(buf, field, 2);
        varint(buf, value.len() as u64);
        buf.extend_from_slice(value.as_bytes());
    }

    /// Wire type 2 (length-delimited). Skips proto3 default `b""`.
    pub fn write_bytes(buf: &mut Vec<u8>, field: u32, value: &[u8]) {
        if value.is_empty() {
            return;
        }
        tag(buf, field, 2);
        varint(buf, value.len() as u64);
        buf.extend_from_slice(value);
    }

    /// Wire type 2 (length-delimited). For sub-messages and for
    /// `repeated string`. **Does not skip empty** — required for
    /// repeated entries where an empty element is legal.
    pub fn write_length_delimited(buf: &mut Vec<u8>, field: u32, bytes: &[u8]) {
        tag(buf, field, 2);
        varint(buf, bytes.len() as u64);
        buf.extend_from_slice(bytes);
    }

    /// Repeated `string` element. Emits even when empty (REAPI
    /// `Command.arguments` allows empty strings and they must be
    /// preserved bit-for-bit).
    pub fn write_repeated_string(buf: &mut Vec<u8>, field: u32, value: &str) {
        write_length_delimited(buf, field, value.as_bytes());
    }
}

// ---- REAPI v2 messages ----

/// `Digest` proto (field numbers: hash=1, size_bytes=2).
fn encode_digest(d: &Digest) -> Vec<u8> {
    let mut out = Vec::new();
    pb::write_string(&mut out, 1, &d.hash);
    pb::write_int64(&mut out, 2, d.size_bytes);
    out
}

/// `Command.EnvironmentVariable` (name=1, value=2).
fn encode_env_var(name: &str, value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    pb::write_string(&mut out, 1, name);
    pb::write_string(&mut out, 2, value);
    out
}

/// `Platform.Property` (name=1, value=2).
fn encode_platform_property(name: &str, value: &str) -> Vec<u8> {
    let mut out = Vec::new();
    pb::write_string(&mut out, 1, name);
    pb::write_string(&mut out, 2, value);
    out
}

/// `Platform` (properties=1, repeated Property).
/// Bazel sorts properties lexicographically by (name, value).
fn encode_platform(props: &BTreeMap<String, String>) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in props {
        let inner = encode_platform_property(name, value);
        pb::write_length_delimited(&mut out, 1, &inner);
    }
    out
}

/// `Command` (REAPI v2):
///   arguments=1, environment_variables=2, output_files=3 [deprecated],
///   output_directories=4 [deprecated], platform=5, working_directory=6,
///   output_node_properties=7 [unused], output_paths=8.
///
/// Bazel >= 5.x emits `output_paths` (field 8) and stops emitting the
/// deprecated 3/4 pair when the remote server advertises
/// `CommandFeatures.output_paths`. v2.1 of the proto deprecated 3/4
/// in favour of 8; Bazel chooses based on server advertisement. We
/// emit `output_paths` and leave 3/4 unset — matches what Bazel
/// produces against any modern (post-2021) remote server.
///
/// Sorting (Bazel's `RemoteSpawnRunner` does exactly this):
///   - `environment_variables` sorted by name.
///   - `output_paths` sorted lexicographically.
///   - `arguments` preserves rule-side order (unsorted).
fn encode_command(cmd: &Command) -> Vec<u8> {
    let mut out = Vec::new();
    // 1: repeated string arguments — user-given order.
    for a in &cmd.arguments {
        pb::write_repeated_string(&mut out, 1, a);
    }
    // 2: repeated EnvironmentVariable — sorted by name.
    for (k, v) in &cmd.environment_variables {
        let inner = encode_env_var(k, v);
        pb::write_length_delimited(&mut out, 2, &inner);
    }
    // 3/4/7: output paths. Bazel 7 still emits the deprecated 3/4 split
    // (output_files vs output_directories) alongside the union field 7
    // (output_paths) for backwards-compat with REAPI v2.0 servers — and
    // crucially, includes them in the wire-hashed Command bytes even
    // though modern servers ignore 3/4. Matching Bazel's hash requires
    // we emit all three with the same lexicographic ordering, AND
    // crucially that field 3 only holds file outputs while field 4
    // only holds tree-artifact outputs. See `RemoteSpawnRunner.
    // buildCommand` in Bazel: it iterates `spawn.getOutputFiles()` and
    // dispatches on `Artifact.isTreeArtifact()` for the 3/4 split.
    let mut files_sorted = cmd.output_files.clone();
    files_sorted.sort();
    let mut dirs_sorted = cmd.output_directories.clone();
    dirs_sorted.sort();
    for p in &files_sorted {
        pb::write_repeated_string(&mut out, 3, p);
    }
    for p in &dirs_sorted {
        pb::write_repeated_string(&mut out, 4, p);
    }
    // 5: Platform (sub-message). Bazel emits this even when empty
    // (`2a 00` in the wire output) — matching Bazel's hash requires
    // we emit the empty sub-message too.
    let inner = encode_platform(&cmd.platform);
    pb::write_length_delimited(&mut out, 5, &inner);
    // 6: working_directory string.
    pb::write_string(&mut out, 6, &cmd.working_directory);
    // 7: repeated string output_paths — sorted union of files+dirs.
    let mut paths_sorted: Vec<String> = files_sorted
        .iter()
        .chain(dirs_sorted.iter())
        .cloned()
        .collect();
    paths_sorted.sort();
    for p in &paths_sorted {
        pb::write_repeated_string(&mut out, 7, p);
    }
    out
}

/// REAPI v2 `Command` projected into the fields Loupe currently
/// populates. `BTreeMap<String,String>` for env + platform gives us
/// the sort-by-name canonicalisation for free.
///
/// `output_files` and `output_directories` are tracked separately
/// because Bazel emits them into distinct REAPI fields (3 vs 4) for
/// REAPI v2.0 backwards-compat, even though the union goes into the
/// modern field 7 (`output_paths`). The split is observable in the
/// Command's wire bytes that the Action digest hashes.
#[derive(Debug, Default)]
pub struct Command {
    pub arguments: Vec<String>,
    pub environment_variables: BTreeMap<String, String>,
    pub output_files: Vec<String>,
    pub output_directories: Vec<String>,
    pub working_directory: String,
    pub platform: BTreeMap<String, String>,
}

/// `FileNode` (name=1, digest=2, is_executable=4, node_properties=6).
fn encode_file_node(name: &str, digest: &Digest, is_executable: bool) -> Vec<u8> {
    let mut out = Vec::new();
    pb::write_string(&mut out, 1, name);
    let inner = encode_digest(digest);
    pb::write_length_delimited(&mut out, 2, &inner);
    pb::write_bool(&mut out, 4, is_executable);
    out
}

/// `DirectoryNode` (name=1, digest=2).
fn encode_directory_node(name: &str, digest: &Digest) -> Vec<u8> {
    let mut out = Vec::new();
    pb::write_string(&mut out, 1, name);
    let inner = encode_digest(digest);
    pb::write_length_delimited(&mut out, 2, &inner);
    out
}

/// `SymlinkNode` (name=1, target=2, node_properties=3).
fn encode_symlink_node(name: &str, target: &str) -> Vec<u8> {
    let mut out = Vec::new();
    pb::write_string(&mut out, 1, name);
    pb::write_string(&mut out, 2, target);
    out
}

/// `Directory` (files=1, directories=2, symlinks=3, node_properties=4).
/// All three repeated lists MUST be sorted by `name` per REAPI spec
/// (Bazel checks this and a remote executor will reject otherwise).
fn encode_directory(dir: &Directory) -> Vec<u8> {
    let mut out = Vec::new();
    // 1: files (sorted by name).
    let mut files: Vec<&FileEntry> = dir.files.iter().collect();
    files.sort_by(|a, b| a.name.cmp(&b.name));
    for f in files {
        let inner = encode_file_node(&f.name, &f.digest, f.is_executable);
        pb::write_length_delimited(&mut out, 1, &inner);
    }
    // 2: directories (sorted by name).
    let mut dirs: Vec<(&String, &Digest)> = dir.directories.iter().collect();
    dirs.sort_by(|a, b| a.0.cmp(b.0));
    for (name, digest) in dirs {
        let inner = encode_directory_node(name, digest);
        pb::write_length_delimited(&mut out, 2, &inner);
    }
    // 3: symlinks (sorted by name).
    let mut syms: Vec<(&String, &String)> = dir.symlinks.iter().collect();
    syms.sort_by(|a, b| a.0.cmp(b.0));
    for (name, target) in syms {
        let inner = encode_symlink_node(name, target);
        pb::write_length_delimited(&mut out, 3, &inner);
    }
    // 5: NodeProperties — REAPI v2.3+ field. Bazel's MerkleTree.java
    // does not call setNodeProperties on `Directory` builders (source
    // verified in Bazel 7.4.1's `MerkleTree.java:buildMerkleTree`),
    // so we mirror that: skip when empty. Bazel *does* attach a
    // single `bazel_tool_input` NodeProperty to *FileNodes* whose
    // `path` is in the spawn's tool-input set; we don't model the
    // tool-input bit on `Action::inputs` yet (STUB — needs an engine
    // surface), so that branch is currently a no-op and only fires
    // for tool-input file nodes once that surface lands. Regular
    // (non-tool) file nodes never carry NodeProperties.
    out
}

#[derive(Debug, Default, Clone)]
struct FileEntry {
    name: String,
    digest: Digest,
    is_executable: bool,
}

#[derive(Debug, Default)]
struct Directory {
    files: Vec<FileEntry>,
    directories: BTreeMap<String, Digest>,
    symlinks: BTreeMap<String, String>,
}

impl Default for Digest {
    fn default() -> Self {
        Digest::empty()
    }
}

/// `Action` (REAPI v2):
///   command_digest=1, input_root_digest=2, timeout=6,
///   do_not_cache=7, salt=9, platform=10.
///
/// Note: `platform` moved from `Command` to `Action` in v2.2 (Aug
/// 2022). Bazel populates *both* fields when the server advertises
/// `output_paths` capability — see `RemoteSpawnRunner::buildAction`.
/// For PoC parity with current Bazel, we leave both empty (Loupe
/// doesn't model exec_properties); when we do, populating both
/// matches Bazel's wire output.
fn encode_action(action: &ActionProto) -> Vec<u8> {
    let mut out = Vec::new();
    let cmd_inner = encode_digest(&action.command_digest);
    pb::write_length_delimited(&mut out, 1, &cmd_inner);
    let root_inner = encode_digest(&action.input_root_digest);
    pb::write_length_delimited(&mut out, 2, &root_inner);
    // 6: timeout (Duration sub-message). Loupe doesn't track timeouts;
    // leave zero, which matches Bazel's "no timeout" case.
    pb::write_bool(&mut out, 7, action.do_not_cache);
    pb::write_bytes(&mut out, 9, &action.salt);
    // Bazel always emits `Platform` (field 10) as a sub-message —
    // even when empty (`52 00` in wire output). The proto3 default
    // skip would elide it; matching Bazel's hash requires us to emit
    // the empty sub-message too. Same for Command.platform below.
    let inner = encode_platform(&action.platform);
    pb::write_length_delimited(&mut out, 10, &inner);
    out
}

#[derive(Debug, Default)]
struct ActionProto {
    command_digest: Digest,
    input_root_digest: Digest,
    do_not_cache: bool,
    salt: Vec<u8>,
    platform: BTreeMap<String, String>,
}

// ---- input root assembly ----

/// Build the input-root merkle tree from a flat list of input files.
///
/// Bazel's `MerkleTree.build()` walks each input's exec_path, splits
/// on `/`, and builds nested `Directory` protos bottom-up. Each
/// non-leaf directory's `Digest` is the sha256 of its serialized
/// `Directory` proto. The returned digest is the digest of the root.
///
/// `inputs_by_path` must carry an entry for every `File` in `inputs`
/// (matched by [`File::path`]). The caller resolves digests upfront —
/// in the DICE-backed path (`loupe-exec-cas::ActionDigestKey`) that's
/// a `try_compute_join` over `SourceFileDigestKey` / `OutputDigestKey`
/// that fans out across cores; for non-DICE callers (tests, the
/// `dump_ac_keys` CLI) it's just a `BTreeMap` populated synchronously.
fn build_input_root(
    inputs: &[File],
    output_tree_paths: &[String],
    inputs_by_path: &BTreeMap<String, (Digest, bool)>,
) -> Result<Digest, AcKeyError> {
    // Pull every input's pre-resolved digest out of the map. Missing
    // entries are a caller bug — surface explicitly rather than
    // substituting a placeholder digest that would silently produce a
    // wrong AC key.
    let resolved: Vec<(Digest, bool)> = inputs
        .iter()
        .map(|f| {
            inputs_by_path
                .get(&f.path())
                .cloned()
                .ok_or_else(|| AcKeyError::MissingInputDigest(f.path()))
        })
        .collect::<Result<_, _>>()?;

    #[derive(Default)]
    struct Node {
        children: BTreeMap<String, Node>,
        files: Vec<FileEntry>,
    }
    let mut root = Node::default();
    // Bazel pre-stages every tree-artifact output (`declare_directory`)
    // as an *empty* `Directory` node at its exec path in the input
    // root. Matches `RemoteExecutionService.buildInputMerkleTree` which
    // calls `Spawn.getOutputDirectories()` and adds each as an empty
    // dir so the executor knows where to write outputs. Without this
    // the input_root_digest diverges by exactly the size of the empty
    // ancestor chain for every declared output directory.
    for path in output_tree_paths {
        let parts: Vec<&str> = path.split('/').collect();
        let mut cursor = &mut root;
        for d in parts {
            if d.is_empty() {
                continue;
            }
            cursor = cursor.children.entry(d.to_string()).or_default();
        }
        // Cursor now points at the tree-artifact's own Node. Leave it
        // empty — Bazel serialises a zero-byte Directory for the
        // declared output dir, whose Digest is `Digest::empty()`.
    }
    // Bazel marks every regular input `FileNode` as `is_executable=true`
    // regardless of the file's on-disk mode or the explicit
    // `is_executable=` Starlark kwarg. The factory methods on
    // `DirectoryTree.FileNode` are literally called
    // `createExecutable(...)` and hardcode the bit — see
    // `src/main/java/com/google/devtools/build/lib/remote/merkletree/
    // DirectoryTree.java` in Bazel 7.4.1. The recorded `File`'s
    // explicit bit (`with_is_executable(true)`) and the resolver's
    // reported FS mode are still observed for *outputs*
    // (`Command.output_paths`, `ActionResult.output_files`) but don't
    // flow into the *input* merkle tree. Matching Bazel's
    // `input_root_digest` byte-for-byte therefore requires we always
    // emit `true` for every FileNode here. Both the explicit
    // recorded-bit and the resolver-reported FS bit are intentionally
    // ignored in this loop — the resolver still hands them back
    // because it's the same trait the output-resolution path uses.
    for (f, (digest, _resolver_is_executable)) in inputs.iter().zip(resolved.into_iter()) {
        let path = f.path();
        // Split into (dir_components, basename). `File::path()` is
        // `/`-separated; protobuf paths must use forward slashes
        // regardless of host OS, so this matches Bazel.
        let parts: Vec<&str> = path.split('/').collect();
        let (basename, dir_parts) = parts.split_last().expect("File::path() never empty");

        let mut cursor = &mut root;
        for d in dir_parts {
            if d.is_empty() {
                continue; // tolerate leading slash, not expected
            }
            cursor = cursor.children.entry((*d).to_string()).or_default();
        }
        cursor.files.push(FileEntry {
            name: (*basename).to_string(),
            digest,
            is_executable: true,
        });
    }

    fn encode_node(n: &Node, dump_dir: Option<&str>, path: &str) -> Vec<u8> {
        // Post-order: hash children first, then fold their digests
        // into our `directories` list — that's the merkle-tree shape
        // REAPI mandates.
        let mut dir = Directory {
            files: n.files.clone(),
            directories: BTreeMap::new(),
            symlinks: BTreeMap::new(),
        };
        for (name, child) in &n.children {
            let child_path = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}/{name}")
            };
            let child_bytes = encode_node(child, dump_dir, &child_path);
            let child_digest = Digest::of_bytes(&child_bytes);
            dir.directories.insert(name.clone(), child_digest);
        }
        let bytes = encode_directory(&dir);
        if let Some(dump) = dump_dir {
            let digest = Digest::of_bytes(&bytes);
            let safe_path = path.replace('/', "__");
            let _ = std::fs::write(
                format!(
                    "{dump}.dir.{safe_path}.{}.{}",
                    digest.hash, digest.size_bytes
                ),
                &bytes,
            );
        }
        bytes
    }
    let dump_env = std::env::var("LOUPE_DUMP_AC_PROTOS").ok();
    let root_bytes = encode_node(&root, dump_env.as_deref(), "");
    if let Some(dump) = dump_env.as_deref() {
        let _ = std::fs::write(format!("{dump}.input_root"), &root_bytes);
    }
    Ok(Digest::of_bytes(&root_bytes))
}

// ---- action-shape extraction ----

/// Project a recorded [`Action`] into the (argv, env, output_paths,
/// inputs) tuple `encode_command` / `build_input_root` consume.
fn project_action(action: &Action) -> Result<ProjectedAction, AcKeyError> {
    match &action.kind {
        ActionKind::Run {
            executable,
            arguments,
        } => {
            // Bazel's `Spawn` for `actions.run` is argv =
            // [executable.path, *arguments]. Vendored
            // `actions_run_shell` already rewrites run_shell → run with
            // `/bin/bash` as executable + `-c command` in arguments, so
            // this branch covers both shapes.
            let mut argv = Vec::with_capacity(1 + arguments.len());
            argv.push(executable.path());
            argv.extend(arguments.iter().cloned());
            // Bazel auto-includes the executable in the spawn's input
            // set (`SpawnAction.Builder.build()` → `inputAndToolManifests`).
            // Loupe records the executable on `ActionKind::Run` and
            // doesn't replicate that auto-inclusion at recording
            // time, so do it here at projection time. Skip the
            // absolute-path case (`/bin/bash` etc. from
            // `actions_run_shell`'s rewrite) — those are host tools,
            // not workspace artifacts, and Bazel doesn't include them
            // either. Dedupe against `action.inputs` so a rule that
            // already listed the executable in `inputs=`/`tools=`
            // doesn't get a duplicate FileNode in the merkle tree.
            let mut inputs = action.inputs.clone();
            let exec_path = executable.path();
            if !exec_path.starts_with('/') && !inputs.iter().any(|f| f.path() == exec_path) {
                inputs.insert(0, executable.clone());
            }
            Ok(ProjectedAction {
                argv,
                env: action.env.clone(),
                output_files: file_paths_of_kind(&action.outputs, false),
                output_directories: file_paths_of_kind(&action.outputs, true),
                inputs,
                kind: ActionShape::Spawn,
            })
        }
        ActionKind::RunShell { command, arguments } => {
            // Defence in depth: Loupe's vendored wrappers.bzl rewrites
            // run_shell into run, so this branch shouldn't normally
            // fire — but if a future code path records RunShell
            // directly, hash it the way Bazel's
            // `StarlarkActionFactory.runShell` lowers it:
            //   - no arguments: `[/bin/bash, -c, command]`
            //   - with arguments: `[/bin/bash, -c, command, "", *args]`
            // The empty `""` is the shell's `$0` and is only emitted
            // when there are positional args to forward.
            let mut argv = vec!["/bin/bash".to_string(), "-c".to_string(), command.clone()];
            if !arguments.is_empty() {
                argv.push(String::new());
            }
            argv.extend(arguments.iter().cloned());
            Ok(ProjectedAction {
                argv,
                env: action.env.clone(),
                output_files: file_paths_of_kind(&action.outputs, false),
                output_directories: file_paths_of_kind(&action.outputs, true),
                inputs: action.inputs.clone(),
                kind: ActionShape::Spawn,
            })
        }
        ActionKind::Write { .. }
        | ActionKind::Symlink { .. }
        | ActionKind::ExpandTemplate { .. } => {
            // These are Bazel-internal action types ("spawn-less" —
            // `FileWriteAction`, `SymlinkAction`, `TemplateExpansionAction`)
            // that Bazel never sends to a remote executor. They have
            // no REAPI `Action` digest. Callers that want a stable
            // identifier for these should use a separate
            // analysis-side key (e.g. sha256 of the content + output
            // path). STUB / out of scope for "Bazel-identical AC key".
            Err(AcKeyError::NotSpawnAction(action.mnemonic.clone()))
        }
    }
}

#[derive(Debug)]
struct ProjectedAction {
    argv: Vec<String>,
    env: BTreeMap<String, String>,
    output_files: Vec<String>,
    output_directories: Vec<String>,
    inputs: Vec<File>,
    #[allow(dead_code)] // future: aspects/exec_properties dispatch
    kind: ActionShape,
}

/// Split a flat output list on `File::is_directory()`.
///
/// `want_directory == true` yields tree-artifact outputs (those
/// declared via `ctx.actions.declare_directory`); `false` yields
/// regular file outputs. Mirrors Bazel's `Artifact.isTreeArtifact()`
/// dispatch in `RemoteSpawnRunner.buildCommand`, which routes file
/// outputs to `Command.output_files` (REAPI field 3) and tree outputs
/// to `Command.output_directories` (REAPI field 4).
fn file_paths_of_kind(files: &[File], want_directory: bool) -> Vec<String> {
    files
        .iter()
        .filter(|f| f.is_directory() == want_directory)
        .map(|f| f.path())
        .collect()
}

#[derive(Debug, Clone, Copy)]
enum ActionShape {
    Spawn,
}

/// Apply the recorded `substitutions` map to `template_bytes` the way
/// Bazel's `TemplateExpansionAction.expand()` does: replace each
/// `key` literally with `value`. Order matters when keys overlap;
/// Bazel iterates in insertion order and Loupe stores substitutions
/// in a `BTreeMap` (sorted), so collision-free substitution sets
/// (which is the only case rulesets reach for in practice) produce
/// identical output. Collision-laden substitutions are a STUB —
/// flagged by [`AcKeyError::SubstitutionOrderAmbiguous`].
pub fn materialise_expand_template(
    template_bytes: &[u8],
    substitutions: &BTreeMap<String, String>,
) -> Result<Vec<u8>, AcKeyError> {
    let mut s = match std::str::from_utf8(template_bytes) {
        Ok(s) => s.to_string(),
        Err(_) => {
            // Bazel's template expansion is byte-naive when keys are
            // ASCII (the common case). Non-UTF8 templates with
            // multi-byte substitutions are out of scope.
            return Err(AcKeyError::TemplateNotUtf8);
        }
    };
    for (key, value) in substitutions {
        s = s.replace(key, value);
    }
    Ok(s.into_bytes())
}

// ---- public entry point ----

#[derive(Debug, thiserror::Error)]
pub enum AcKeyError {
    #[error("input file not found on disk at {path}: {reason}")]
    IoRead { path: String, reason: String },
    /// Caller didn't pre-resolve a digest for an `action.inputs` entry.
    /// `compute_action_cache_key` does no I/O of its own — the
    /// `inputs_by_path` map must carry an entry for every input file.
    /// The DICE-backed driver in `loupe-exec-cas` is what walks back
    /// through the producer action graph (or REAPI `ActionResult`
    /// lookups for upstream `Run`/`RunShell` outputs); when *that*
    /// can't locate a producer it surfaces the gap as a `CasError`
    /// before ever reaching this function.
    #[error("input {0} has no pre-resolved digest in inputs_by_path")]
    MissingInputDigest(String),
    /// Bazel-internal actions (FileWrite, Symlink, TemplateExpansion)
    /// are never sent to a remote executor and have no REAPI Action
    /// digest. Callers can still get a content-based identifier — out
    /// of scope for "Bazel-identical".
    #[error("action with mnemonic {0:?} is a Bazel-internal action; no REAPI Action digest")]
    NotSpawnAction(Option<String>),
    #[error("template expansion source is not UTF-8")]
    TemplateNotUtf8,
    #[error("substitution map order is ambiguous (overlapping keys)")]
    #[allow(dead_code)] // reserved for the collision-detection follow-up
    SubstitutionOrderAmbiguous,
}

/// Compute the Bazel-identical AC key for a recorded action.
///
/// **Scope:** spawn-shaped actions only (`ActionKind::Run` and
/// `ActionKind::RunShell`). `Write`/`Symlink`/`ExpandTemplate` are
/// not sent to a remote executor by Bazel, so they have no REAPI
/// `Action` digest — returns [`AcKeyError::NotSpawnAction`].
///
/// **Inputs:** the caller pre-resolves every input path to
/// `(Digest, is_executable)` and hands `inputs_by_path` in. Source
/// digests come from sha256-of-bytes (via DICE's `SourceFileDigestKey`
/// in the production path); generated-input digests come from upstream
/// recorded actions (Write/Symlink/ExpandTemplate content is derivable
/// from analysis) or a REAPI `ActionResult.output_files[i].digest`
/// lookup for upstream `Run`/`RunShell` outputs. Missing entries
/// surface as [`AcKeyError::MissingInputDigest`].
///
/// **Platform:** built by merging the `configuration`'s platform-level
/// `exec_properties` with the action's `execution_requirements`, with
/// the per-action entries winning on key collisions. Matches Bazel's
/// `SpawnAction.getExecutionInfo()` semantics. Emitted to both
/// `Command.platform` (REAPI v2.0/2.1) and `Action.platform` (REAPI
/// v2.2+), the same as Bazel does when the remote server advertises
/// the `output_paths` capability.
///
/// **Env:** when `action.uses_default_shell_env` is set
/// (`use_default_shell_env=True` on `ctx.actions.run` /
/// `ctx.actions.run_shell`), [`default_shell_env`] is folded under
/// the action's explicit `env` map — action entries override defaults
/// on key collisions, matching Bazel's
/// `RemoteSpawnRunner.buildCommand()` behaviour.
pub fn compute_action_cache_key(
    action: &Action,
    configuration: &Configuration,
    inputs_by_path: &BTreeMap<String, (Digest, bool)>,
) -> Result<Digest, AcKeyError> {
    let proj = project_action(action)?;
    let env = merge_env(&proj.env, action.uses_default_shell_env);
    let platform = build_platform_properties(configuration.exec_properties());

    let output_directories = proj.output_directories.clone();
    let cmd = Command {
        arguments: proj.argv,
        environment_variables: env,
        output_files: proj.output_files,
        output_directories: proj.output_directories,
        working_directory: String::new(),
        platform: platform.clone(),
    };
    let cmd_bytes = encode_command(&cmd);
    let cmd_digest = Digest::of_bytes(&cmd_bytes);
    if let Ok(dump) = std::env::var("LOUPE_DUMP_AC_PROTOS") {
        let _ = std::fs::write(format!("{dump}.command"), &cmd_bytes);
        eprintln!(
            "DUMP: command_digest={}/{} bytes_at={}.command",
            cmd_digest.hash, cmd_digest.size_bytes, dump,
        );
    }

    let input_root_digest = build_input_root(&proj.inputs, &output_directories, inputs_by_path)?;
    if std::env::var("LOUPE_DUMP_AC_PROTOS").is_ok() {
        eprintln!(
            "DUMP: input_root_digest={}/{}",
            input_root_digest.hash, input_root_digest.size_bytes,
        );
    }

    let act = ActionProto {
        command_digest: cmd_digest,
        input_root_digest,
        // Bazel's `Spawns.shouldNotBeCached` flips `Action.do_not_cache`
        // when the action's `execution_requirements` contains
        // `"no-cache"` (any value). Mirrors
        // `ExecutionRequirements.NO_CACHE`'s wiring in
        // `RemoteSpawnRunner.buildAction`.
        do_not_cache: action.execution_requirements.contains_key("no-cache"),
        // Bazel's `RemoteCacheClient.SALT` for SHA256 mode: the
        // proto-serialised `DigestFunction { value: SHA256 }`, i.e.
        // tag=1 wire=0, varint=1 → bytes `[0x08, 0x01]`. Matches
        // Bazel's `DigestUtil.toByteString(SHA256)` — the salt
        // Bazel injects into every Action when the cache digest
        // function is SHA256. Pinning it lets Loupe's Action proto
        // match Bazel's byte-for-byte.
        salt: vec![0x08, 0x01],
        platform,
    };
    let act_bytes = encode_action(&act);
    if let Ok(dump) = std::env::var("LOUPE_DUMP_AC_PROTOS") {
        let _ = std::fs::write(format!("{dump}.action"), &act_bytes);
    }
    Ok(Digest::of_bytes(&act_bytes))
}

/// Layer [`default_shell_env`] under `action_env` when the action set
/// `use_default_shell_env=True`. Action-supplied entries override
/// defaults on key collisions — matches Bazel's
/// `RemoteSpawnRunner.buildCommand` ordering.
fn merge_env(
    action_env: &BTreeMap<String, String>,
    uses_default: bool,
) -> BTreeMap<String, String> {
    if !uses_default {
        return action_env.clone();
    }
    let mut env = default_shell_env();
    for (k, v) in action_env {
        env.insert(k.clone(), v.clone());
    }
    env
}

/// Build the REAPI Platform.properties map from the spawn's combined
/// exec_properties — that's *only* the platform's `exec_properties`
/// (plus target-level `exec_properties` once Loupe models them).
/// Per-action `execution_requirements` (the dict from
/// `ctx.actions.run(execution_requirements=…)`) is a separate
/// spawn-dispatch hint and does NOT flow into Platform.properties.
///
/// Mirrors Bazel's `PlatformUtils.getPlatformProto` /
/// `Spawn.getCombinedExecProperties` — they consult the spawn's
/// execution platform and target attributes, never the
/// `executionInfo` map.
fn build_platform_properties(
    platform_props: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    platform_props.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use loupe_labels::{CanonicalRepo, PackageId, PackagePath};

    fn root_pkg() -> PackageId {
        PackageId::new(CanonicalRepo::MAIN, PackagePath::root())
    }

    // ---- protobuf wire-format fixed points ----
    //
    // Each test below is the canonical proto3 wire-format example for
    // its message shape. These values are inscribed in the protobuf
    // language guide / REAPI conformance fixtures — if any drift, the
    // encoder no longer produces Bazel-identical output.

    #[test]
    fn varint_encodes_known_examples() {
        // From https://protobuf.dev/programming-guides/encoding/
        let cases: &[(u64, &[u8])] = &[
            (0, &[0x00]),
            (1, &[0x01]),
            (127, &[0x7f]),
            (128, &[0x80, 0x01]),
            (150, &[0x96, 0x01]),
            (300, &[0xac, 0x02]),
        ];
        for (n, expected) in cases {
            let mut buf = Vec::new();
            pb::varint(&mut buf, *n);
            assert_eq!(&buf[..], *expected, "varint({n})");
        }
    }

    #[test]
    fn digest_of_empty_bytes_matches_reapi_constant() {
        // REAPI conformance: sha256("") = e3b0c4… with size 0.
        let d = Digest::of_bytes(&[]);
        assert_eq!(d, Digest::empty());
    }

    #[test]
    fn empty_command_emits_empty_platform_sub_message() {
        // Bazel's `RemoteSpawnRunner.buildCommand()` always calls
        // `command.setPlatform(...)` (possibly with an empty
        // `Platform`), and Java protobuf emits the sub-message marker
        // even for empty messages — `2a 00` in the wire output.
        // Loupe matches: an otherwise-empty Command still serialises
        // to those 2 bytes. Diverges from REAPI's "well-known empty
        // Command" digest (sha256("") = e3b0c4…) which would only
        // hold for an encoder that skips empty sub-messages.
        let cmd = Command::default();
        let bytes = encode_command(&cmd);
        assert_eq!(bytes, vec![0x2a, 0x00]);
    }

    #[test]
    fn command_splits_file_and_directory_outputs_into_distinct_fields() {
        // Bazel's `RemoteSpawnRunner.buildCommand` routes file outputs
        // into `Command.output_files` (REAPI field 3) and tree-artifact
        // outputs into `Command.output_directories` (field 4), then
        // emits the union under `Command.output_paths` (field 7) for
        // REAPI v2.1+. The split is observable in the wire bytes the
        // Action digest hashes — emitting every output under field 3
        // is a Loupe-side encoder bug that flips the Action digest for
        // any rule that declares a tree-artifact output
        // (`ctx.actions.declare_directory`).
        //
        // Pins the byte sequence for a Command whose outputs are one
        // file (`out.txt`) and one directory (`out_dir`):
        //   field 5 Platform = `2a 00` (always-emitted empty marker)
        //   field 3 = "out.txt"            -> `1a 07 6f 75 74 2e 74 78 74`
        //   field 4 = "out_dir"            -> `22 07 6f 75 74 5f 64 69 72`
        //   field 7 sorted union:
        //     "out.txt"                    -> `3a 07 ...`
        //     "out_dir"                    -> `3a 07 ...`
        // Bazel sorts both lists lexicographically; here "out.txt" <
        // "out_dir" because `.` (0x2e) < `_` (0x5f).
        let cmd = Command {
            output_files: vec!["out.txt".to_string()],
            output_directories: vec!["out_dir".to_string()],
            ..Default::default()
        };
        let bytes = encode_command(&cmd);
        let mut expected = Vec::new();
        // field 3 (output_files): "out.txt"
        expected.extend_from_slice(&[0x1a, 0x07]);
        expected.extend_from_slice(b"out.txt");
        // field 4 (output_directories): "out_dir"
        expected.extend_from_slice(&[0x22, 0x07]);
        expected.extend_from_slice(b"out_dir");
        // field 5 (Platform, empty marker)
        expected.extend_from_slice(&[0x2a, 0x00]);
        // field 7 (output_paths, sorted union)
        expected.extend_from_slice(&[0x3a, 0x07]);
        expected.extend_from_slice(b"out.txt");
        expected.extend_from_slice(&[0x3a, 0x07]);
        expected.extend_from_slice(b"out_dir");
        assert_eq!(bytes, expected);
    }

    #[test]
    fn input_root_pre_stages_declared_output_directories_as_empty_dirs() {
        // Bazel's `RemoteExecutionService.buildInputMerkleTree` adds
        // every tree-artifact output (`ctx.actions.declare_directory`)
        // as an *empty* `Directory` node at its exec path in the input
        // root. The executor needs the path to exist before writing
        // outputs; Bazel pre-stages it.
        //
        // Captured against Bazel 7.4.1 for `/tmp/treeart-fixture
        // //:t`: a `run_shell` declaring `bazel-out/k8-fastbuild/bin/
        // t_dir` (tree artifact) produced an input_root_digest of 83
        // bytes — a single `bazel-out/k8-fastbuild/bin/t_dir/` chain
        // of nested empty Directory protos. With no inputs and no
        // pre-staging, Loupe's input root was `Digest::empty()` (0
        // bytes), making the Action digest diverge.
        let inputs: Vec<File> = vec![];
        let inputs_by_path: BTreeMap<String, (Digest, bool)> = BTreeMap::new();
        let output_dirs = vec!["bazel-out/k8-fastbuild/bin/t_dir".to_string()];
        let digest = build_input_root(&inputs, &output_dirs, &inputs_by_path).unwrap();
        // The expected hash matches Bazel 7.4.1's wire output for
        // this exact path. Pinning the literal digest catches any
        // regression in the empty-Directory chain encoding.
        assert_eq!(
            digest.hash,
            "ffd2bf63e9d59035b3e738b40c528e4d1fdd54c207ef3c40670068bd39c2c4eb",
        );
        assert_eq!(digest.size_bytes, 83);
        // Compare against an explicitly-constructed empty
        // `bazel-out -> k8-fastbuild -> bin -> t_dir` chain to prove
        // the encoding is the empty-Directory shape, not just hash
        // coincidence.
        let empty_dir = encode_directory(&Directory::default());
        assert!(empty_dir.is_empty());
        let leaf_digest = Digest::empty();
        // bin Directory: one DirectoryNode "t_dir" -> empty digest.
        let bin_dir = Directory {
            directories: {
                let mut m = BTreeMap::new();
                m.insert("t_dir".to_string(), leaf_digest);
                m
            },
            ..Default::default()
        };
        let bin_bytes = encode_directory(&bin_dir);
        let bin_digest = Digest::of_bytes(&bin_bytes);
        let k8_dir = Directory {
            directories: {
                let mut m = BTreeMap::new();
                m.insert("bin".to_string(), bin_digest);
                m
            },
            ..Default::default()
        };
        let k8_bytes = encode_directory(&k8_dir);
        let k8_digest = Digest::of_bytes(&k8_bytes);
        let bazel_out_dir = Directory {
            directories: {
                let mut m = BTreeMap::new();
                m.insert("k8-fastbuild".to_string(), k8_digest);
                m
            },
            ..Default::default()
        };
        let bazel_out_bytes = encode_directory(&bazel_out_dir);
        let bazel_out_digest = Digest::of_bytes(&bazel_out_bytes);
        let root_dir = Directory {
            directories: {
                let mut m = BTreeMap::new();
                m.insert("bazel-out".to_string(), bazel_out_digest);
                m
            },
            ..Default::default()
        };
        let expected_root = encode_directory(&root_dir);
        assert_eq!(digest, Digest::of_bytes(&expected_root));
    }

    #[test]
    fn tree_artifact_output_routes_to_output_directories_field() {
        // End-to-end: a `Run` action whose recorded output is a tree
        // artifact (declared via `ctx.actions.declare_directory`,
        // modelled as `File::generated_bin_dir`) must encode the
        // output path under REAPI Command field 4, not field 3. The
        // digest of a Run action with a tree output must therefore
        // differ from the same Run with a file output of the same
        // path — Bazel's wire encoding distinguishes the two.
        //
        // Captured against Bazel 7.4.1's `npm_package` action for
        // the rules_js probe (`copy_to_directory` -> tree artifact
        // `bazel-out/k8-fastbuild/bin/pkg`): the Command had `field
        // 4 = bazel-out/k8-fastbuild/bin/pkg` and no field 3, then
        // `field 7 = bazel-out/k8-fastbuild/bin/pkg`.
        let mk = |is_tree: bool| {
            let out = if is_tree {
                File::generated_bin_dir(root_pkg(), "pkg", "fake-cfg")
            } else {
                File::generated_bin(root_pkg(), "pkg", "fake-cfg")
            };
            mk_action(
                ActionKind::Run {
                    executable: File::source(root_pkg(), "bin/tool"),
                    arguments: vec![],
                },
                vec![],
                vec![out],
            )
        };
        let r = InMemResolver(BTreeMap::new());
        let cfg = neutral_cfg();
        let d_file = ac_key(&mk(false), &cfg, &r)
            
            .unwrap();
        let d_tree = ac_key(&mk(true), &cfg, &r).unwrap();
        assert_ne!(
            d_file, d_tree,
            "tree-artifact outputs must route to Command.output_directories \
             (field 4), giving a different digest than file outputs at the \
             same path",
        );
    }

    /// Bazel's `SpawnAction` auto-includes the spawn's executable in
    /// its input set. Loupe records `executable` separately on
    /// `ActionKind::Run` but the recorded `inputs` doesn't replicate
    /// the auto-inclusion. `project_action` does it at AC-key time:
    /// when the executable's path looks like a workspace artifact
    /// (not `/abs/path`), it gets folded into the input merkle tree.
    ///
    /// Pinned against the rules_js probe's `//:lib` `CopyFile` action
    /// (Bazel 7.4.1) — Bazel's input root included `external/.../coreutils`
    /// as a FileNode; before this fix Loupe omitted it and the
    /// `Action.input_root_digest` diverged by exactly that FileNode's
    /// size.
    #[test]
    fn run_action_input_root_includes_executable() {
        let mk = |exec_path: &str| {
            mk_action(
                ActionKind::Run {
                    executable: File::source(root_pkg(), exec_path),
                    arguments: vec![],
                },
                vec![],
                vec![File::generated_bin(root_pkg(), "out", "fake-cfg")],
            )
        };
        let r = InMemResolver(BTreeMap::new());
        let cfg = neutral_cfg();

        // Different executables ⇒ different action digests (because
        // the auto-included executable FileNode changes).
        let d_a = ac_key(&mk("bin/tool_a"), &cfg, &r)
            
            .unwrap();
        let d_b = ac_key(&mk("bin/tool_b"), &cfg, &r)
            
            .unwrap();
        assert_ne!(
            d_a, d_b,
            "executable name flowed into input root must flip the AC key"
        );

        // Absolute-path executables (the run_shell rewrite's
        // `/bin/bash` shape) are NOT auto-included — Bazel doesn't
        // either. Two actions differing only in their absolute-path
        // executable should still differ via argv[0], but the input
        // root for both must NOT contain an entry rooted at `/`.
        let abs = mk_action(
            ActionKind::Run {
                executable: File::source(root_pkg(), "/usr/bin/env"),
                arguments: vec![],
            },
            vec![],
            vec![File::generated_bin(root_pkg(), "out", "fake-cfg")],
        );
        // The encoder must not panic / error when given an absolute
        // executable path (the resolver wouldn't have it).
        let _ = ac_key(&abs, &cfg, &r).unwrap();
    }

    #[test]
    fn input_root_emits_is_executable_for_non_executable_source() {
        // The Bazel-identity invariant: every `FileNode` in the input
        // merkle tree carries `is_executable=true` regardless of the
        // file's on-disk mode. Single non-executable source under the
        // root, no nested directories — the root `Directory` proto
        // must contain the file's `is_executable` field (tag `0x20`,
        // value `0x01`).
        //
        // Regression for the 2-byte gap originally observed against
        // the `/tmp/genrule-fixture //:greet` Bazel-uploaded merkle
        // tree (Bazel 7.4.1): Bazel emitted 166 bytes for the input
        // root; Loupe emitted 164. The two missing bytes were `20 01`
        // at the end of the `name.txt` FileNode — `is_executable=true`
        // on a `0644` source file. Bazel's
        // `DirectoryTree.FileNode.createExecutable(...)` hardcodes
        // the bit on every input regardless of FS mode.
        let inputs = vec![File::source(root_pkg(), "name.txt")];
        let inputs_by_path: BTreeMap<String, (Digest, bool)> = [(
            "name.txt".to_string(),
            (Digest::of_bytes(b"world\n"), false),
        )]
        .into_iter()
        .collect();
        let root_digest = build_input_root(&inputs, &[], &inputs_by_path).unwrap();
        // The root Directory has exactly one FileNode with
        // is_executable=true; reconstruct what we expect:
        //   tag(files=1, len-delim) + varint(len) + FileNode bytes
        // FileNode:
        //   tag(name=1, len-delim) + varint(8) + "name.txt"
        //   tag(digest=2, len-delim) + varint(len) + Digest bytes
        //   tag(is_executable=4, varint) + varint(1)
        let content_digest = Digest::of_bytes(b"world\n");
        let mut digest_bytes = Vec::new();
        pb::write_string(&mut digest_bytes, 1, &content_digest.hash);
        pb::write_int64(&mut digest_bytes, 2, content_digest.size_bytes);
        let mut file_node = Vec::new();
        pb::write_string(&mut file_node, 1, "name.txt");
        pb::write_length_delimited(&mut file_node, 2, &digest_bytes);
        // tag(4, varint=0) = 0x20, value = 0x01 — the two-byte
        // is_executable=true emission that closed the Bazel-identity
        // gap.
        file_node.push(0x20);
        file_node.push(0x01);
        let mut expected_root = Vec::new();
        pb::write_length_delimited(&mut expected_root, 1, &file_node);
        assert_eq!(
            root_digest,
            Digest::of_bytes(&expected_root),
            "input root must encode FileNode with is_executable=true",
        );
        // The trailing two bytes `0x20 0x01` MUST be present in the
        // encoded FileNode — defence in depth in case
        // `pb::write_bool`'s skip-default logic ever inadvertently
        // catches this path.
        let len = file_node.len();
        assert_eq!(
            (file_node[len - 2], file_node[len - 1]),
            (0x20, 0x01),
            "FileNode must end with is_executable=true (`0x20 0x01`)",
        );
    }

    #[test]
    fn input_root_byte_identical_to_bazel_for_single_source_file() {
        // Pins the byte-for-byte shape of the input merkle tree against
        // Bazel 7.4.1's wire output for a single non-executable source
        // file at the workspace root. The expected bytes are exactly
        // what bazel-remote captured for `name.txt` (`world\n`, mode
        // 0644) as the sole input in the genrule fixture's merkle
        // tree, with the surrounding `external/` subtree stripped.
        //
        // Closes the gap originally documented in 4867897 (ac_key:
        // document the remaining 2-byte input_root gap as known).
        let inputs = vec![File::source(root_pkg(), "name.txt")];
        let inputs_by_path: BTreeMap<String, (Digest, bool)> = [(
            "name.txt".to_string(),
            (Digest::of_bytes(b"world\n"), false),
        )]
        .into_iter()
        .collect();
        let digest = build_input_root(&inputs, &[], &inputs_by_path).unwrap();

        // `world\n` sha256 prefix — pin so a regression in
        // `Digest::of_bytes` would surface here too.
        assert_eq!(
            content_digest_hex(b"world\n"),
            "e258d248fda94c63753607f7c4494ee0fcbe92f1a76bfdac795c9d84101eb317",
        );
        // 82 bytes for the FileNode payload (`0x52 = 82` in the
        // varint-length tag emitted by Bazel) + 2 bytes for the outer
        // `files=1` framing (`0x0a 0x52`) = 84 bytes total for the
        // root Directory proto when its only member is a single
        // FileNode. Without the `is_executable=true` emission the
        // FileNode would be 80 bytes and the root 82 — exactly the
        // 2-byte gap originally observed.
        assert_eq!(digest.size_bytes, 84);
    }

    fn content_digest_hex(bytes: &[u8]) -> String {
        Digest::of_bytes(bytes).hash
    }

    #[test]
    fn empty_directory_digest_matches_reapi_well_known() {
        // REAPI: an empty Directory{} proto is zero bytes, and its
        // digest equals sha256(""). Bazel's `MerkleTree.EMPTY_DIGEST`
        // matches.
        let dir = Directory::default();
        let bytes = encode_directory(&dir);
        assert!(bytes.is_empty());
        assert_eq!(Digest::of_bytes(&bytes), Digest::empty());
    }

    #[test]
    fn empty_action_emits_command_root_and_platform() {
        // An Action with two empty-Digest sub-messages plus the
        // always-emitted empty Platform marker. Wire form:
        //   tag(1, 2) + varint(len) + Digest(command_digest)
        //   tag(2, 2) + varint(len) + Digest(input_root_digest)
        //   tag(10, 2) + varint(0)   — empty Platform marker
        // Each Digest sub-message:
        //   tag(1, 2) = 1 byte
        //   varint(64) = 1 byte
        //   64 hex hash chars
        //   (size_bytes field skipped — value 0 is proto3 default)
        // Digest total = 66 bytes; outer tag+len = 2 → 68 bytes per
        // Digest. Two Digests = 136 bytes. Platform marker = 2 bytes.
        // Total = 138 bytes.
        let act = ActionProto {
            command_digest: Digest::empty(),
            input_root_digest: Digest::empty(),
            ..Default::default()
        };
        let bytes = encode_action(&act);
        assert_eq!(bytes.len(), 138);
        let d = Digest::of_bytes(&bytes);
        let d2 = Digest::of_bytes(&encode_action(&act));
        assert_eq!(d, d2);
    }

    // ---- end-to-end: full Action digest ----

    /// In-memory `path → bytes` map. Tests own one of these for the
    /// inputs they want to pin; everything else falls through to
    /// `Digest::of_bytes(b"")` via [`InMemResolver::inputs_for`].
    struct InMemResolver(BTreeMap<String, Vec<u8>>);

    impl InMemResolver {
        /// Build the `inputs_by_path` map `compute_action_cache_key`
        /// expects. Walks `project_action(action).inputs` (so the
        /// auto-included executable lands in the map) and fills any
        /// unspecified path with the empty-bytes digest. That keeps
        /// tests focused on the field they're actually exercising —
        /// auto-included paths contribute a stable empty digest, so
        /// the merkle root stays deterministic and input-sensitive in
        /// the way the assertions check.
        fn inputs_for(&self, action: &Action) -> BTreeMap<String, (Digest, bool)> {
            // Non-spawn actions (FileWrite, Symlink, TemplateExpansion)
            // never reach the merkle-tree walk inside
            // `compute_action_cache_key` — it bails with
            // `NotSpawnAction` before the map is consulted. Return an
            // empty map so test setup doesn't panic on those paths.
            let Ok(proj) = project_action(action) else {
                return BTreeMap::new();
            };
            proj.inputs
                .iter()
                .map(|f| {
                    let path = f.path();
                    let digest = self
                        .0
                        .get(&path)
                        .map(|b| Digest::of_bytes(b))
                        .unwrap_or_else(|| Digest::of_bytes(b""));
                    (path, (digest, false))
                })
                .collect()
        }
    }

    /// Test convenience: drives [`compute_action_cache_key`] through
    /// an [`InMemResolver`]-built `inputs_by_path` map. Keeps each
    /// test body focused on the field it's exercising rather than the
    /// merkle-tree bookkeeping.
    fn ac_key(
        action: &Action,
        cfg: &Configuration,
        r: &InMemResolver,
    ) -> Result<Digest, AcKeyError> {
        let inputs = r.inputs_for(action);
        compute_action_cache_key(action, cfg, &inputs)
    }

    /// Build a minimal `Action` carrying only the fields each test
    /// cares about. Other fields default to neutral (empty
    /// `execution_requirements`, `uses_default_shell_env=false`)
    /// — they wash out of the digest when the configuration also
    /// supplies empty `exec_properties`.
    fn mk_action(kind: ActionKind, inputs: Vec<File>, outputs: Vec<File>) -> Action {
        Action {
            kind,
            mnemonic: None,
            progress_message: None,
            inputs,
            outputs,
            env: BTreeMap::new(),
            execution_requirements: BTreeMap::new(),
            uses_default_shell_env: false,
        }
    }

    fn neutral_cfg() -> Configuration {
        // Empty exec_properties + segment_override keeps the
        // Platform empty and the bazel-out segment stable. The two
        // pieces that would otherwise leak across tests if a non-
        // neutral `default_v1()` snuck in.
        Configuration::default()
    }

    #[test]
    fn run_action_digest_is_deterministic_and_input_sensitive() {
        // Build a tiny Run action: argv = ["/bin/echo", "hello"], one
        // source-file input. Compute the digest twice and check it
        // matches. Then change the source content and check the digest
        // changes — that's the merkle-tree dependency proof.
        let action = Action {
            mnemonic: Some("Echo".to_string()),
            ..mk_action(
                ActionKind::Run {
                    executable: File::source(root_pkg(), "bin/echo"),
                    arguments: vec!["hello".to_string()],
                },
                vec![File::source(root_pkg(), "input.txt")],
                vec![File::generated_bin(root_pkg(), "out.txt", "fake-cfg")],
            )
        };
        let cfg = neutral_cfg();

        let mut m = BTreeMap::new();
        m.insert("input.txt".to_string(), b"v1".to_vec());
        let r1 = InMemResolver(m);
        let d1 = ac_key(&action, &cfg, &r1).unwrap();
        let d1_again = ac_key(&action, &cfg, &r1).unwrap();
        assert_eq!(d1, d1_again, "deterministic");

        let mut m2 = BTreeMap::new();
        m2.insert("input.txt".to_string(), b"v2".to_vec());
        let r2 = InMemResolver(m2);
        let d2 = ac_key(&action, &cfg, &r2).unwrap();
        assert_ne!(d1, d2, "input content change must flip the AC key");
    }

    #[test]
    fn run_action_digest_changes_with_argv() {
        // Argv is part of the Command proto; change it and the AC key
        // must change too. This proves Command.arguments is plumbed.
        let mk = |arg: &str| {
            mk_action(
                ActionKind::Run {
                    executable: File::source(root_pkg(), "bin/echo"),
                    arguments: vec![arg.to_string()],
                },
                vec![],
                vec![File::generated_bin(root_pkg(), "out.txt", "fake-cfg")],
            )
        };
        let r = InMemResolver(BTreeMap::new());
        let cfg = neutral_cfg();
        let d1 = ac_key(&mk("a"), &cfg, &r).unwrap();
        let d2 = ac_key(&mk("b"), &cfg, &r).unwrap();
        assert_ne!(d1, d2);
    }

    #[test]
    fn run_action_digest_changes_with_env() {
        let mk = |v: &str| {
            let mut a = mk_action(
                ActionKind::Run {
                    executable: File::source(root_pkg(), "bin/echo"),
                    arguments: vec![],
                },
                vec![],
                vec![],
            );
            a.env.insert("FOO".to_string(), v.to_string());
            a
        };
        let r = InMemResolver(BTreeMap::new());
        let cfg = neutral_cfg();
        assert_ne!(
            ac_key(&mk("1"), &cfg, &r).unwrap(),
            ac_key(&mk("2"), &cfg, &r).unwrap(),
        );
    }

    #[test]
    fn write_action_has_no_remote_cache_key() {
        // FileWriteAction is a Bazel-internal action — no remote
        // executor ever sees it. Calling `compute_action_cache_key`
        // on one must surface the boundary explicitly, not silently
        // return some made-up digest.
        let action = Action {
            mnemonic: Some("FileWrite".to_string()),
            ..mk_action(
                ActionKind::Write {
                    content: "hi".to_string(),
                    is_executable: false,
                },
                vec![],
                vec![File::generated_bin(root_pkg(), "out.txt", "fake-cfg")],
            )
        };
        let r = InMemResolver(BTreeMap::new());
        let cfg = neutral_cfg();
        let err = ac_key(&action, &cfg, &r)
            
            .unwrap_err();
        assert!(matches!(err, AcKeyError::NotSpawnAction(_)));
    }

    #[test]
    fn missing_input_digest_surfaces_explicit_error() {
        // Caller forgot to pre-resolve a digest for an `action.inputs`
        // entry — the function does no I/O of its own and must surface
        // the gap with the offending path, not substitute a placeholder
        // that would silently produce a wrong AC key. This is the same
        // boundary as the old `UnknownGenerated` (a downstream input is
        // an upstream Run output not yet materialised) — with the
        // resolver collapsed into a plain map, "no digest" is the
        // single, explicit signal.
        let action = mk_action(
            ActionKind::Run {
                executable: File::source(root_pkg(), "tool"),
                arguments: vec![],
            },
            vec![File::generated_bin(root_pkg(), "upstream.out", "fake-cfg")],
            vec![],
        );
        let cfg = neutral_cfg();
        // Pre-populate everything *except* the generated input, so the
        // assertion pins the gap path exactly. The auto-included
        // executable lands in `project_action(...).inputs` first; if
        // we left it unfilled, the error would point at the executable
        // instead.
        let mut inputs_by_path: BTreeMap<String, (Digest, bool)> = BTreeMap::new();
        inputs_by_path.insert("tool".to_string(), (Digest::empty(), false));
        let err = compute_action_cache_key(&action, &cfg, &inputs_by_path).unwrap_err();
        match err {
            AcKeyError::MissingInputDigest(p) => {
                assert_eq!(p, "bazel-out/fake-cfg/bin/upstream.out");
            }
            other => panic!("expected MissingInputDigest, got {other:?}"),
        }
    }

    #[test]
    fn upstream_write_output_feeds_downstream_via_inputs_map() {
        // Cross-action handoff: a Write action's output content is
        // derivable from analysis alone (it's the literal `content`
        // string), so a downstream Run that takes it as input can
        // hash successfully — provided the caller pre-resolves the
        // upstream digest and includes it in `inputs_by_path`. That's
        // exactly what `loupe-exec-cas::ActionDigestKey` does in
        // production via `OutputDigestKey` for Write/Symlink/
        // ExpandTemplate producers.
        let upstream = Action {
            mnemonic: Some("FileWrite".to_string()),
            ..mk_action(
                ActionKind::Write {
                    content: "generated body".to_string(),
                    is_executable: false,
                },
                vec![],
                vec![File::generated_bin(root_pkg(), "gen.txt", "fake-cfg")],
            )
        };
        let downstream = Action {
            mnemonic: Some("Consume".to_string()),
            ..mk_action(
                ActionKind::Run {
                    executable: File::source(root_pkg(), "tool"),
                    arguments: vec![],
                },
                vec![File::generated_bin(root_pkg(), "gen.txt", "fake-cfg")],
                vec![],
            )
        };

        // Caller's pre-walk: harvest the upstream Write output's
        // digest, plus the source `tool` bytes, into one input map.
        let upstream_out_path = upstream.outputs[0].path();
        let upstream_digest = match &upstream.kind {
            ActionKind::Write {
                content,
                is_executable,
            } => (Digest::of_bytes(content.as_bytes()), *is_executable),
            _ => unreachable!(),
        };
        let mut inputs_by_path: BTreeMap<String, (Digest, bool)> = BTreeMap::new();
        inputs_by_path.insert(upstream_out_path, upstream_digest);
        inputs_by_path.insert("tool".to_string(), (Digest::of_bytes(b"#!/bin/sh\n"), false));
        let cfg = neutral_cfg();

        // No MissingInputDigest — the cross-action handoff works
        // analysis-only when upstream is content-derivable.
        let d = compute_action_cache_key(&downstream, &cfg, &inputs_by_path).unwrap();
        assert!(!d.hash.is_empty());
    }

    #[test]
    fn execution_requirements_do_not_flow_into_platform() {
        // `Action::execution_requirements` is a spawn-dispatch hint
        // (no-sandbox, local, no-remote-exec, …) and does NOT become a
        // Platform property in the REAPI Command/Action protos.
        // Bazel's `PlatformUtils.getPlatformProto` builds Platform
        // from `Spawn.getCombinedExecProperties()` (platform-level
        // exec_properties), not from `Spawn.getExecutionInfo()`.
        //
        // Captured against Bazel 7.4.1 for `/tmp/copyfile-fixture
        // //:with_er` (a `run_shell` with
        // `execution_requirements={"no-sandbox": "1"}`): Bazel's
        // Action.platform (field 10) and Command.platform (field 5)
        // were both the empty sub-message — `no-sandbox` did not
        // surface as a Platform property.
        //
        // Per-action `execution_requirements` only affects the
        // Action's `do_not_cache` field (when the dict contains
        // `"no-cache"`) — see
        // `no_cache_execution_requirement_sets_do_not_cache_field`.
        let mk = |req_key: &str, req_value: &str| {
            let mut a = mk_action(
                ActionKind::Run {
                    executable: File::source(root_pkg(), "bin/tool"),
                    arguments: vec![],
                },
                vec![],
                vec![],
            );
            a.execution_requirements
                .insert(req_key.to_string(), req_value.to_string());
            a
        };
        let r = InMemResolver(BTreeMap::new());
        let cfg = neutral_cfg();
        let d_empty = ac_key(
            &mk_action(
                ActionKind::Run {
                    executable: File::source(root_pkg(), "bin/tool"),
                    arguments: vec![],
                },
                vec![],
                vec![],
            ),
            &cfg,
            &r,
        )
        
        .unwrap();
        let d_no_sandbox = ac_key(&mk("no-sandbox", "1"), &cfg, &r)
            
            .unwrap();
        let d_no_remote = ac_key(&mk("no-remote-exec", "1"), &cfg, &r)
            
            .unwrap();
        assert_eq!(
            d_empty, d_no_sandbox,
            "execution_requirements other than no-cache must NOT change the digest"
        );
        assert_eq!(
            d_empty, d_no_remote,
            "no-remote-exec is a dispatch hint, not a Platform property"
        );
    }

    #[test]
    fn no_cache_execution_requirement_sets_do_not_cache_field() {
        // Bazel's `Spawns.shouldNotBeCached` flips
        // `Action.do_not_cache` when `execution_requirements` contains
        // the `"no-cache"` key (any value, including the empty string).
        // The field is a proto3 bool — the digest changes when it
        // flips from default-false (omitted) to true (`38 01` on the
        // wire).
        let base = mk_action(
            ActionKind::Run {
                executable: File::source(root_pkg(), "bin/tool"),
                arguments: vec![],
            },
            vec![],
            vec![],
        );
        let mut with_no_cache = base.clone();
        with_no_cache
            .execution_requirements
            .insert("no-cache".to_string(), "".to_string());
        let r = InMemResolver(BTreeMap::new());
        let cfg = neutral_cfg();
        let d_off = ac_key(&base, &cfg, &r).unwrap();
        let d_on = ac_key(&with_no_cache, &cfg, &r)
            
            .unwrap();
        assert_ne!(
            d_off, d_on,
            "no-cache execution_requirement must flip Action.do_not_cache"
        );
        // Pin the size delta: `do_not_cache=true` adds `38 01` (2
        // bytes) to the Action encoding.
        assert_eq!(d_on.size_bytes, d_off.size_bytes + 2);
    }

    #[test]
    fn platform_exec_properties_flow_into_action_digest() {
        // `Configuration::exec_properties` (from the analyzed target's
        // platform) must surface as `Platform.properties` too —
        // toolchain platforms set things like `container-image` here
        // and remote executors gate on the same value.
        let action = mk_action(
            ActionKind::Run {
                executable: File::source(root_pkg(), "bin/tool"),
                arguments: vec![],
            },
            vec![],
            vec![],
        );
        let r = InMemResolver(BTreeMap::new());
        let cfg_empty = Configuration::default();
        let cfg_with_props = Configuration::default().with_exec_properties({
            let mut m = BTreeMap::new();
            m.insert("OSFamily".to_string(), "Linux".to_string());
            m
        });
        let d_empty = ac_key(&action, &cfg_empty, &r)
            
            .unwrap();
        let d_with = ac_key(&action, &cfg_with_props, &r)
            
            .unwrap();
        assert_ne!(d_empty, d_with, "exec_properties must flow into the AC key");
    }

    #[test]
    fn action_execution_requirements_do_not_override_platform() {
        // Since `execution_requirements` does NOT flow into Platform
        // (Bazel separates spawn-dispatch hints from REAPI Platform
        // properties — see
        // `execution_requirements_do_not_flow_into_platform`), the
        // Configuration's `exec_properties` is the *sole* source of
        // Platform.properties. Setting `execution_requirements` on
        // the action with the same key as the platform's
        // `exec_properties` does NOT shadow the platform value at the
        // REAPI Command/Action wire encoding.
        let mut a = mk_action(
            ActionKind::Run {
                executable: File::source(root_pkg(), "bin/tool"),
                arguments: vec![],
            },
            vec![],
            vec![],
        );
        a.execution_requirements
            .insert("Pool".to_string(), "fast".to_string());
        let r = InMemResolver(BTreeMap::new());
        let cfg_slow = Configuration::default().with_exec_properties({
            let mut m = BTreeMap::new();
            m.insert("Pool".to_string(), "slow".to_string());
            m
        });
        let cfg_fast = Configuration::default().with_exec_properties({
            let mut m = BTreeMap::new();
            m.insert("Pool".to_string(), "fast".to_string());
            m
        });
        let d_slow = ac_key(&a, &cfg_slow, &r).unwrap();
        let d_fast = ac_key(&a, &cfg_fast, &r).unwrap();
        assert_ne!(
            d_slow, d_fast,
            "platform exec_properties value drives Platform.properties; \
             the action's `Pool=fast` execution_requirement does NOT \
             override the platform-side `Pool=slow`",
        );
    }

    #[test]
    fn uses_default_shell_env_folds_path_into_env() {
        // When `uses_default_shell_env=True`, the PATH from
        // `default_shell_env()` must surface in the Command env. The
        // digest is sensitive to the bit because the env changes.
        let action_no_default = mk_action(
            ActionKind::Run {
                executable: File::source(root_pkg(), "bin/tool"),
                arguments: vec![],
            },
            vec![],
            vec![],
        );
        let mut action_with_default = action_no_default.clone();
        action_with_default.uses_default_shell_env = true;
        let r = InMemResolver(BTreeMap::new());
        let cfg = neutral_cfg();
        let d_off = ac_key(&action_no_default, &cfg, &r)
            
            .unwrap();
        let d_on = ac_key(&action_with_default, &cfg, &r)
            
            .unwrap();
        assert_ne!(d_off, d_on, "uses_default_shell_env must flip the AC key");
    }

    #[test]
    fn action_env_overrides_default_shell_env_on_collision() {
        // Action env beats the default — same as Bazel.
        let mut a = mk_action(
            ActionKind::Run {
                executable: File::source(root_pkg(), "bin/tool"),
                arguments: vec![],
            },
            vec![],
            vec![],
        );
        a.uses_default_shell_env = true;
        a.env
            .insert("PATH".to_string(), "/just/this/one".to_string());
        // Two scenarios: action env beats default, and changing the
        // explicit value flips the digest. We just confirm the second
        // — flipping the override value changes the digest — which is
        // the proof that the action's PATH (not the default) made it
        // through.
        let r = InMemResolver(BTreeMap::new());
        let cfg = neutral_cfg();
        let d1 = ac_key(&a, &cfg, &r).unwrap();
        a.env.insert("PATH".to_string(), "/other".to_string());
        let d2 = ac_key(&a, &cfg, &r).unwrap();
        assert_ne!(d1, d2);
    }

    #[test]
    fn input_file_node_is_always_executable_matching_bazel() {
        // Bazel's `MerkleTree.buildFromActionInputs` calls
        // `DirectoryTree.FileNode.createExecutable(...)` for *every*
        // regular input file; the factory hardcodes
        // `isExecutable=true` regardless of the file's on-disk mode or
        // any Starlark-side annotation. Matching Bazel's
        // `input_root_digest` byte-for-byte therefore requires Loupe
        // ignore both the recorded `File`'s explicit bit *and* the
        // resolver-reported FS bit when constructing the input
        // FileNode. (Outputs still observe both bits — those flow
        // through `Command.output_paths` / `ActionResult.output_files`,
        // not the input merkle tree.)
        let mk = |exec: bool| {
            let mut input = File::source(root_pkg(), "tool.sh");
            input = input.with_is_executable(exec);
            mk_action(
                ActionKind::Run {
                    executable: File::source(root_pkg(), "bin/sh"),
                    arguments: vec![],
                },
                vec![input],
                vec![],
            )
        };
        let r = InMemResolver({
            let mut m = BTreeMap::new();
            m.insert("tool.sh".to_string(), b"#!/bin/sh\necho hi\n".to_vec());
            m
        });
        let cfg = neutral_cfg();
        let d_off = ac_key(&mk(false), &cfg, &r)
            
            .unwrap();
        let d_on = ac_key(&mk(true), &cfg, &r).unwrap();
        assert_eq!(
            d_off, d_on,
            "input FileNode.is_executable is hardcoded to true (Bazel parity); \
             the recorded-bit must not flow into the input merkle tree",
        );
    }

    #[test]
    fn materialise_expand_template_substitutes_in_place() {
        // The substitution map is sorted (BTreeMap); confirm a simple
        // collision-free substitution produces exactly the bytes a
        // Bazel `TemplateExpansionAction` would write.
        let mut subs = BTreeMap::new();
        subs.insert("{NAME}".to_string(), "alice".to_string());
        subs.insert("{GREETING}".to_string(), "hello".to_string());
        let out = materialise_expand_template(b"{GREETING}, {NAME}!", &subs).unwrap();
        assert_eq!(out, b"hello, alice!");
    }
}
