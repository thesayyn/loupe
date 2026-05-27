//! REAPI v2 response decoders.
//!
//! Mirrors `loupe_core::ac_key::pb`'s hand-rolled wire-format
//! encoders. Decode-side is more permissive: skip unknown fields so a
//! REAPI v3+ server with new tags doesn't break the path.
//!
//! Only the fields the rest of the crate consumes are exposed on the
//! returned structs. Tag numbers are pinned to REAPI v2 from
//! `build.bazel.remote.execution.v2.remote_execution.proto` (stable
//! since v2.0).

use allocative::Allocative;
use loupe_core::ac_key::Digest;

use crate::error::CasError;

/// REAPI `ActionResult`. Fields beyond what Loupe currently reads
/// (`execution_metadata`, `node_properties`, etc.) are dropped during
/// decode — they don't affect digest propagation.
#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub struct ActionResult {
    /// Files produced by the action, keyed by their `output_paths`
    /// entry from the original Command. Each carries its content
    /// digest — the value `OutputDigestKey` plumbs into the merkle
    /// tree of downstream actions.
    pub output_files: Vec<OutputFile>,
    /// Tree artifacts produced by `ctx.actions.declare_directory`.
    /// Each carries the `tree_digest` of a serialized REAPI `Tree`
    /// proto enumerating directory contents.
    pub output_directories: Vec<OutputDirectory>,
    pub exit_code: i32,
    pub stdout_digest: Option<Digest>,
    pub stderr_digest: Option<Digest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub struct OutputFile {
    pub path: String,
    pub digest: Digest,
    pub is_executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub struct OutputDirectory {
    pub path: String,
    /// Digest of the serialized `Tree` proto enumerating directory
    /// contents. v1 just records this; consumers that need to walk
    /// the directory listing fetch + decode the `Tree` blob from CAS
    /// separately.
    pub tree_digest: Digest,
}

// ---- wire-format reader primitives ----

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn eof(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn varint(&mut self) -> Result<u64, CasError> {
        let mut result: u64 = 0;
        let mut shift = 0u32;
        loop {
            if self.pos >= self.buf.len() {
                return Err(CasError::Decode("truncated varint".to_string()));
            }
            let byte = self.buf[self.pos];
            self.pos += 1;
            result |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
            if shift >= 64 {
                return Err(CasError::Decode("varint > 64 bits".to_string()));
            }
        }
    }

    /// Read tag varint and split into (field_number, wire_type).
    fn tag(&mut self) -> Result<(u32, u8), CasError> {
        let v = self.varint()?;
        let field = (v >> 3) as u32;
        let wire = (v & 0x7) as u8;
        Ok((field, wire))
    }

    fn len_delim(&mut self) -> Result<&'a [u8], CasError> {
        let len = self.varint()? as usize;
        if self.pos + len > self.buf.len() {
            return Err(CasError::Decode(format!(
                "length-delimited overrun: need {len}, have {}",
                self.buf.len() - self.pos
            )));
        }
        let out = &self.buf[self.pos..self.pos + len];
        self.pos += len;
        Ok(out)
    }

    /// Skip a field whose wire type we recognise but whose body we
    /// don't care about. Required for unknown / unconsumed fields so
    /// the cursor advances past them.
    fn skip(&mut self, wire: u8) -> Result<(), CasError> {
        match wire {
            0 => {
                self.varint()?;
            }
            1 => {
                // 64-bit fixed
                if self.pos + 8 > self.buf.len() {
                    return Err(CasError::Decode("truncated fixed64".to_string()));
                }
                self.pos += 8;
            }
            2 => {
                self.len_delim()?;
            }
            5 => {
                // 32-bit fixed
                if self.pos + 4 > self.buf.len() {
                    return Err(CasError::Decode("truncated fixed32".to_string()));
                }
                self.pos += 4;
            }
            _ => return Err(CasError::Decode(format!("unsupported wire type {wire}"))),
        }
        Ok(())
    }
}

// ---- typed message decoders ----

/// `Digest { hash: 1, size_bytes: 2 }`.
pub fn decode_digest(bytes: &[u8]) -> Result<Digest, CasError> {
    let mut r = Reader::new(bytes);
    let mut hash = String::new();
    let mut size_bytes: i64 = 0;
    while !r.eof() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, 2) => {
                let s = r.len_delim()?;
                hash = std::str::from_utf8(s)
                    .map_err(|e| CasError::Decode(format!("Digest.hash not UTF-8: {e}")))?
                    .to_string();
            }
            (2, 0) => {
                size_bytes = r.varint()? as i64;
            }
            (_, w) => r.skip(w)?,
        }
    }
    Ok(Digest { hash, size_bytes })
}

/// `OutputFile { path: 1, digest: 2, is_executable: 4, contents: 5 }`.
/// Field 5 (`contents`) is the inline-content shortcut REAPI v2
/// allows for tiny outputs; we ignore it (consumers fetch from CAS by
/// digest instead — same result, one extra hop).
pub fn decode_output_file(bytes: &[u8]) -> Result<OutputFile, CasError> {
    let mut r = Reader::new(bytes);
    let mut path = String::new();
    let mut digest = Digest::empty();
    let mut is_executable = false;
    while !r.eof() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, 2) => {
                path = std::str::from_utf8(r.len_delim()?)
                    .map_err(|e| CasError::Decode(format!("OutputFile.path not UTF-8: {e}")))?
                    .to_string();
            }
            (2, 2) => {
                digest = decode_digest(r.len_delim()?)?;
            }
            (4, 0) => {
                is_executable = r.varint()? != 0;
            }
            (_, w) => r.skip(w)?,
        }
    }
    Ok(OutputFile {
        path,
        digest,
        is_executable,
    })
}

/// REAPI `Tree`: the serialised proto a tree artifact's `tree_digest`
/// resolves to in CAS. Carries the root `Directory` plus every nested
/// child `Directory`. v1 consumes this to look up an individual file
/// inside a tree-artifact output when a downstream action's input path
/// is `<tree_artifact_root>/<sub/path>`.
#[derive(Debug, Clone, PartialEq, Eq, Allocative, Default)]
pub struct Tree {
    pub root: Directory,
    pub children: Vec<Directory>,
}

/// REAPI `Directory`. Only the fields needed for tree-artifact lookup
/// are surfaced: `files`, `directories`, and `symlinks`. Each
/// `DirectoryNode` carries the digest of a *child* `Directory` whose
/// bytes appear elsewhere in `Tree.children`.
#[derive(Debug, Clone, PartialEq, Eq, Allocative, Default)]
pub struct Directory {
    pub files: Vec<FileNode>,
    pub directories: Vec<DirectoryNode>,
    pub symlinks: Vec<SymlinkNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub struct FileNode {
    pub name: String,
    pub digest: Digest,
    pub is_executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub struct DirectoryNode {
    pub name: String,
    pub digest: Digest,
}

#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub struct SymlinkNode {
    pub name: String,
    pub target: String,
}

/// `OutputDirectory { path: 1, tree_digest: 3, root_directory_digest: 5 }`.
/// `tree_digest` is the field we surface; the root_directory_digest
/// (REAPI v2.2 addition) is decoded only to skip cleanly.
pub fn decode_output_directory(bytes: &[u8]) -> Result<OutputDirectory, CasError> {
    let mut r = Reader::new(bytes);
    let mut path = String::new();
    let mut tree_digest = Digest::empty();
    while !r.eof() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, 2) => {
                path = std::str::from_utf8(r.len_delim()?)
                    .map_err(|e| CasError::Decode(format!("OutputDirectory.path not UTF-8: {e}")))?
                    .to_string();
            }
            (3, 2) => {
                tree_digest = decode_digest(r.len_delim()?)?;
            }
            (_, w) => r.skip(w)?,
        }
    }
    Ok(OutputDirectory { path, tree_digest })
}

/// `FileNode { name: 1, digest: 2, is_executable: 4, node_properties: 6 }`.
pub fn decode_file_node(bytes: &[u8]) -> Result<FileNode, CasError> {
    let mut r = Reader::new(bytes);
    let mut name = String::new();
    let mut digest = Digest::empty();
    let mut is_executable = false;
    while !r.eof() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, 2) => {
                name = std::str::from_utf8(r.len_delim()?)
                    .map_err(|e| CasError::Decode(format!("FileNode.name not UTF-8: {e}")))?
                    .to_string();
            }
            (2, 2) => {
                digest = decode_digest(r.len_delim()?)?;
            }
            (4, 0) => {
                is_executable = r.varint()? != 0;
            }
            (_, w) => r.skip(w)?,
        }
    }
    Ok(FileNode {
        name,
        digest,
        is_executable,
    })
}

/// `DirectoryNode { name: 1, digest: 2 }`.
pub fn decode_directory_node(bytes: &[u8]) -> Result<DirectoryNode, CasError> {
    let mut r = Reader::new(bytes);
    let mut name = String::new();
    let mut digest = Digest::empty();
    while !r.eof() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, 2) => {
                name = std::str::from_utf8(r.len_delim()?)
                    .map_err(|e| CasError::Decode(format!("DirectoryNode.name not UTF-8: {e}")))?
                    .to_string();
            }
            (2, 2) => {
                digest = decode_digest(r.len_delim()?)?;
            }
            (_, w) => r.skip(w)?,
        }
    }
    Ok(DirectoryNode { name, digest })
}

/// `SymlinkNode { name: 1, target: 2, node_properties: 3 }`.
pub fn decode_symlink_node(bytes: &[u8]) -> Result<SymlinkNode, CasError> {
    let mut r = Reader::new(bytes);
    let mut name = String::new();
    let mut target = String::new();
    while !r.eof() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, 2) => {
                name = std::str::from_utf8(r.len_delim()?)
                    .map_err(|e| CasError::Decode(format!("SymlinkNode.name not UTF-8: {e}")))?
                    .to_string();
            }
            (2, 2) => {
                target = std::str::from_utf8(r.len_delim()?)
                    .map_err(|e| CasError::Decode(format!("SymlinkNode.target not UTF-8: {e}")))?
                    .to_string();
            }
            (_, w) => r.skip(w)?,
        }
    }
    Ok(SymlinkNode { name, target })
}

/// `Directory { files: 1, directories: 2, symlinks: 3, node_properties: 4 }`.
pub fn decode_directory(bytes: &[u8]) -> Result<Directory, CasError> {
    let mut r = Reader::new(bytes);
    let mut out = Directory::default();
    while !r.eof() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, 2) => out.files.push(decode_file_node(r.len_delim()?)?),
            (2, 2) => out.directories.push(decode_directory_node(r.len_delim()?)?),
            (3, 2) => out.symlinks.push(decode_symlink_node(r.len_delim()?)?),
            (_, w) => r.skip(w)?,
        }
    }
    Ok(out)
}

/// REAPI `Tree { root: 1, children: 2 }`. The bytes a tree artifact's
/// `OutputDirectory.tree_digest` resolves to in CAS.
pub fn decode_tree(bytes: &[u8]) -> Result<Tree, CasError> {
    let mut r = Reader::new(bytes);
    let mut out = Tree::default();
    while !r.eof() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (1, 2) => out.root = decode_directory(r.len_delim()?)?,
            (2, 2) => out.children.push(decode_directory(r.len_delim()?)?),
            (_, w) => r.skip(w)?,
        }
    }
    Ok(out)
}

/// Walk a decoded `Tree` to resolve a sub-path inside it. `sub_path`
/// is the requested path relative to the tree-artifact root, with `/`
/// as separator and no leading slash. Returns the matching `FileNode`,
/// or `None` if the path doesn't resolve to a file in the tree
/// (missing entry, symlink, or path component naming a directory).
///
/// Symlinks inside the tree are not followed (REAPI clients leave that
/// to the executor); a request that lands on a symlink returns `None`.
pub fn tree_lookup<'a>(tree: &'a Tree, sub_path: &str) -> Option<&'a FileNode> {
    let parts: Vec<&str> = sub_path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }
    // Build a digest → Directory lookup over children so we can hop
    // between nested directories.
    // The Tree's children carry one Directory per nested level. The
    // parent's DirectoryNode references the child by *its* serialised
    // digest, not by name — so build an index by hex hash. Encoding is
    // the same wire shape Bazel used when writing the Tree blob.
    let by_digest: std::collections::HashMap<String, &Directory> = tree
        .children
        .iter()
        .map(|d| (encode_directory_digest_hash(d), d))
        .collect();
    let (last, dirs) = parts.split_last()?;
    let mut cursor = &tree.root;
    for seg in dirs {
        let dn = cursor.directories.iter().find(|d| d.name == *seg)?;
        cursor = *by_digest.get(&dn.digest.hash)?;
    }
    cursor.files.iter().find(|f| f.name == *last)
}

/// Hash an encoded `Directory` for use as a lookup key inside
/// [`tree_lookup`]. The digest stored in `Tree.children` is computed
/// by Bazel as `sha256(serialised(Directory))` — encoding-side mirrors
/// `loupe_core::ac_key::encode_directory`. We don't depend on
/// `loupe_core` here because the field set we need (FileNode +
/// DirectoryNode + SymlinkNode) is independently shaped by the
/// decoded structs above; sharing the encoder via a public re-export
/// would couple the two crates more than the lookup is worth.
fn encode_directory_digest_hash(d: &Directory) -> String {
    use sha2::{Digest as _, Sha256};
    let bytes = encode_directory_for_digest(d);
    let out = Sha256::new_with_prefix(&bytes).finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Mirror of `loupe_core::ac_key::encode_directory` for the fields the
/// decoder surfaces. Kept in lockstep with the encoder side; the
/// `Tree` proto in CAS was written by Bazel using the same wire layout
/// `ac_key::pb` reproduces.
fn encode_directory_for_digest(d: &Directory) -> Vec<u8> {
    fn varint(buf: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            buf.push(((v & 0x7f) as u8) | 0x80);
            v >>= 7;
        }
        buf.push(v as u8);
    }
    fn tag(buf: &mut Vec<u8>, field: u32, wire: u8) {
        varint(buf, ((field as u64) << 3) | (wire as u64));
    }
    fn write_string(buf: &mut Vec<u8>, field: u32, value: &str) {
        if value.is_empty() {
            return;
        }
        tag(buf, field, 2);
        varint(buf, value.len() as u64);
        buf.extend_from_slice(value.as_bytes());
    }
    fn write_int64(buf: &mut Vec<u8>, field: u32, value: i64) {
        if value == 0 {
            return;
        }
        tag(buf, field, 0);
        varint(buf, value as u64);
    }
    fn write_bool(buf: &mut Vec<u8>, field: u32, value: bool) {
        if !value {
            return;
        }
        tag(buf, field, 0);
        varint(buf, 1);
    }
    fn write_length_delimited(buf: &mut Vec<u8>, field: u32, bytes: &[u8]) {
        tag(buf, field, 2);
        varint(buf, bytes.len() as u64);
        buf.extend_from_slice(bytes);
    }
    fn encode_digest(d: &Digest) -> Vec<u8> {
        let mut out = Vec::new();
        write_string(&mut out, 1, &d.hash);
        write_int64(&mut out, 2, d.size_bytes);
        out
    }
    let mut out = Vec::new();
    // 1: files (sorted by name).
    let mut files: Vec<&FileNode> = d.files.iter().collect();
    files.sort_by(|a, b| a.name.cmp(&b.name));
    for f in files {
        let mut inner = Vec::new();
        write_string(&mut inner, 1, &f.name);
        let dbytes = encode_digest(&f.digest);
        write_length_delimited(&mut inner, 2, &dbytes);
        write_bool(&mut inner, 4, f.is_executable);
        write_length_delimited(&mut out, 1, &inner);
    }
    // 2: directories (sorted by name).
    let mut dirs: Vec<&DirectoryNode> = d.directories.iter().collect();
    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    for dn in dirs {
        let mut inner = Vec::new();
        write_string(&mut inner, 1, &dn.name);
        let dbytes = encode_digest(&dn.digest);
        write_length_delimited(&mut inner, 2, &dbytes);
        write_length_delimited(&mut out, 2, &inner);
    }
    // 3: symlinks (sorted by name).
    let mut syms: Vec<&SymlinkNode> = d.symlinks.iter().collect();
    syms.sort_by(|a, b| a.name.cmp(&b.name));
    for s in syms {
        let mut inner = Vec::new();
        write_string(&mut inner, 1, &s.name);
        write_string(&mut inner, 2, &s.target);
        write_length_delimited(&mut out, 3, &inner);
    }
    out
}

/// `ActionResult { output_files: 2, output_directories: 3, exit_code: 4,
///                 stdout_digest: 6, stderr_digest: 8 }`.
/// Skips: output_symlinks (10), execution_metadata (9), node_properties
/// branches — none affect digest propagation.
pub fn decode_action_result(bytes: &[u8]) -> Result<ActionResult, CasError> {
    let mut r = Reader::new(bytes);
    let mut out = ActionResult {
        output_files: Vec::new(),
        output_directories: Vec::new(),
        exit_code: 0,
        stdout_digest: None,
        stderr_digest: None,
    };
    while !r.eof() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (2, 2) => {
                out.output_files.push(decode_output_file(r.len_delim()?)?);
            }
            (3, 2) => {
                out.output_directories
                    .push(decode_output_directory(r.len_delim()?)?);
            }
            (4, 0) => {
                out.exit_code = r.varint()? as i32;
            }
            (6, 2) => {
                out.stdout_digest = Some(decode_digest(r.len_delim()?)?);
            }
            (8, 2) => {
                out.stderr_digest = Some(decode_digest(r.len_delim()?)?);
            }
            (_, w) => r.skip(w)?,
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hand-rolled encoders mirror `loupe_core::ac_key::pb`. Round-
    // tripping our own encode→decode catches field-number / wire-type
    // mistakes before any real REAPI server gets involved.

    fn varint(buf: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            buf.push(((v & 0x7f) as u8) | 0x80);
            v >>= 7;
        }
        buf.push(v as u8);
    }

    fn tag(buf: &mut Vec<u8>, field: u32, wire: u8) {
        varint(buf, ((field as u64) << 3) | (wire as u64));
    }

    fn encode_digest(d: &Digest) -> Vec<u8> {
        let mut out = Vec::new();
        if !d.hash.is_empty() {
            tag(&mut out, 1, 2);
            varint(&mut out, d.hash.len() as u64);
            out.extend_from_slice(d.hash.as_bytes());
        }
        if d.size_bytes != 0 {
            tag(&mut out, 2, 0);
            varint(&mut out, d.size_bytes as u64);
        }
        out
    }

    fn encode_output_file(f: &OutputFile) -> Vec<u8> {
        let mut out = Vec::new();
        if !f.path.is_empty() {
            tag(&mut out, 1, 2);
            varint(&mut out, f.path.len() as u64);
            out.extend_from_slice(f.path.as_bytes());
        }
        let dbytes = encode_digest(&f.digest);
        tag(&mut out, 2, 2);
        varint(&mut out, dbytes.len() as u64);
        out.extend_from_slice(&dbytes);
        if f.is_executable {
            tag(&mut out, 4, 0);
            varint(&mut out, 1);
        }
        out
    }

    fn encode_action_result(ar: &ActionResult) -> Vec<u8> {
        let mut out = Vec::new();
        for f in &ar.output_files {
            let fbytes = encode_output_file(f);
            tag(&mut out, 2, 2);
            varint(&mut out, fbytes.len() as u64);
            out.extend_from_slice(&fbytes);
        }
        if ar.exit_code != 0 {
            tag(&mut out, 4, 0);
            varint(&mut out, ar.exit_code as u64);
        }
        out
    }

    #[test]
    fn digest_round_trip() {
        let d = Digest {
            hash: "abc123".repeat(10).chars().take(64).collect::<String>(),
            size_bytes: 1234,
        };
        let bytes = encode_digest(&d);
        let back = decode_digest(&bytes).unwrap();
        assert_eq!(d, back);
    }

    #[test]
    fn empty_digest_decodes_to_empty_hash_zero_size() {
        let back = decode_digest(&[]).unwrap();
        assert_eq!(back.hash, "");
        assert_eq!(back.size_bytes, 0);
    }

    #[test]
    fn output_file_round_trip_with_exec_bit() {
        let f = OutputFile {
            path: "bazel-out/k8-fastbuild/bin/pkg/out.bin".to_string(),
            digest: Digest::of_bytes(b"hello"),
            is_executable: true,
        };
        let back = decode_output_file(&encode_output_file(&f)).unwrap();
        assert_eq!(f, back);
    }

    #[test]
    fn action_result_round_trip_two_outputs() {
        let ar = ActionResult {
            output_files: vec![
                OutputFile {
                    path: "a/out".to_string(),
                    digest: Digest::of_bytes(b"a"),
                    is_executable: false,
                },
                OutputFile {
                    path: "b/out".to_string(),
                    digest: Digest::of_bytes(b"bb"),
                    is_executable: true,
                },
            ],
            output_directories: vec![],
            exit_code: 0,
            stdout_digest: None,
            stderr_digest: None,
        };
        let back = decode_action_result(&encode_action_result(&ar)).unwrap();
        assert_eq!(ar, back);
    }

    #[test]
    fn unknown_field_is_skipped_silently() {
        // REAPI v3+ field tags we don't know must not break decode.
        let mut bytes = encode_action_result(&ActionResult {
            output_files: vec![],
            output_directories: vec![],
            exit_code: 7,
            stdout_digest: None,
            stderr_digest: None,
        });
        // Append a varint at unknown field 99.
        tag(&mut bytes, 99, 0);
        varint(&mut bytes, 42);
        let back = decode_action_result(&bytes).unwrap();
        assert_eq!(back.exit_code, 7);
    }

    fn encode_file_node(f: &FileNode) -> Vec<u8> {
        let mut out = Vec::new();
        if !f.name.is_empty() {
            tag(&mut out, 1, 2);
            varint(&mut out, f.name.len() as u64);
            out.extend_from_slice(f.name.as_bytes());
        }
        let dbytes = encode_digest(&f.digest);
        tag(&mut out, 2, 2);
        varint(&mut out, dbytes.len() as u64);
        out.extend_from_slice(&dbytes);
        if f.is_executable {
            tag(&mut out, 4, 0);
            varint(&mut out, 1);
        }
        out
    }

    fn encode_directory_node(dn: &DirectoryNode) -> Vec<u8> {
        let mut out = Vec::new();
        if !dn.name.is_empty() {
            tag(&mut out, 1, 2);
            varint(&mut out, dn.name.len() as u64);
            out.extend_from_slice(dn.name.as_bytes());
        }
        let dbytes = encode_digest(&dn.digest);
        tag(&mut out, 2, 2);
        varint(&mut out, dbytes.len() as u64);
        out.extend_from_slice(&dbytes);
        out
    }

    fn encode_directory_msg(d: &Directory) -> Vec<u8> {
        // Mirror of ac_key::encode_directory: files=1, directories=2,
        // symlinks=3, all sorted by name.
        let mut out = Vec::new();
        let mut files: Vec<&FileNode> = d.files.iter().collect();
        files.sort_by(|a, b| a.name.cmp(&b.name));
        for f in files {
            let inner = encode_file_node(f);
            tag(&mut out, 1, 2);
            varint(&mut out, inner.len() as u64);
            out.extend_from_slice(&inner);
        }
        let mut dirs: Vec<&DirectoryNode> = d.directories.iter().collect();
        dirs.sort_by(|a, b| a.name.cmp(&b.name));
        for dn in dirs {
            let inner = encode_directory_node(dn);
            tag(&mut out, 2, 2);
            varint(&mut out, inner.len() as u64);
            out.extend_from_slice(&inner);
        }
        out
    }

    fn encode_tree_msg(t: &Tree) -> Vec<u8> {
        let mut out = Vec::new();
        let rbytes = encode_directory_msg(&t.root);
        tag(&mut out, 1, 2);
        varint(&mut out, rbytes.len() as u64);
        out.extend_from_slice(&rbytes);
        for c in &t.children {
            let cbytes = encode_directory_msg(c);
            tag(&mut out, 2, 2);
            varint(&mut out, cbytes.len() as u64);
            out.extend_from_slice(&cbytes);
        }
        out
    }

    #[test]
    fn tree_decode_then_lookup_finds_top_level_file() {
        // A producer wrote a tree artifact with one file at the root.
        // Encode it via our local encoder (mirrors what Bazel writes
        // to CAS), decode through `decode_tree`, then walk to the
        // file by name. The decoded FileNode's digest must round-trip.
        let digest = Digest::of_bytes(b"hello");
        let tree = Tree {
            root: Directory {
                files: vec![FileNode {
                    name: "x.js".to_string(),
                    digest: digest.clone(),
                    is_executable: false,
                }],
                directories: vec![],
                symlinks: vec![],
            },
            children: vec![],
        };
        let bytes = encode_tree_msg(&tree);
        let decoded = decode_tree(&bytes).unwrap();
        let fnode = tree_lookup(&decoded, "x.js").expect("found");
        assert_eq!(fnode.digest, digest);
    }

    #[test]
    fn tree_decode_then_lookup_walks_nested_directory() {
        // Tree with a nested child Directory. The root's
        // DirectoryNode references the child by its serialised digest,
        // which is how Bazel stitches the proto together. `tree_lookup`
        // must hash the child and find the match.
        let inner_digest = Digest::of_bytes(b"world");
        let child = Directory {
            files: vec![FileNode {
                name: "y.js".to_string(),
                digest: inner_digest.clone(),
                is_executable: true,
            }],
            directories: vec![],
            symlinks: vec![],
        };
        let child_hash = encode_directory_digest_hash(&child);
        let child_bytes = encode_directory_msg(&child);
        let root = Directory {
            files: vec![],
            directories: vec![DirectoryNode {
                name: "sub".to_string(),
                digest: Digest {
                    hash: child_hash,
                    size_bytes: child_bytes.len() as i64,
                },
            }],
            symlinks: vec![],
        };
        let tree = Tree {
            root,
            children: vec![child],
        };
        let bytes = encode_tree_msg(&tree);
        let decoded = decode_tree(&bytes).unwrap();
        let fnode = tree_lookup(&decoded, "sub/y.js").expect("found");
        assert_eq!(fnode.digest, inner_digest);
        assert!(fnode.is_executable);
    }

    #[test]
    fn tree_lookup_returns_none_for_missing_path() {
        let tree = Tree::default();
        assert!(tree_lookup(&tree, "anything").is_none());
    }

    #[test]
    fn truncated_varint_is_decode_error() {
        // High bit set on the last byte → reader keeps going past EOF.
        let bytes = vec![0xff, 0xff];
        let err = decode_digest(&bytes).unwrap_err();
        assert!(matches!(err, CasError::Decode(_)));
    }
}
