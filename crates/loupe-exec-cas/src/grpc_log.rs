//! Decoder for Bazel's `--experimental_remote_grpc_log` output.
//!
//! Bazel writes a stream of length-delimited `LogEntry` protos (one
//! per gRPC call) to the file named on that flag. Each entry's
//! `details.get_action_result.request.action_digest` is the REAPI
//! Action digest Bazel computed for some spawn; pair that against
//! Loupe's [`crate::ActionDigestKey`] output for the same action and
//! you have a byte-for-byte Bazel-identity check.
//!
//! Schema source:
//! `bazel/src/main/protobuf/remote_execution_log.proto` (Bazel 9
//! pin) and `bazelbuild/remote-apis/build/bazel/remote/execution/v2/remote_execution.proto`.
//! Tag numbers below are pinned to those files.
//!
//! Like [`crate::proto`], this decoder skips unknown fields cleanly —
//! a Bazel 10+ grpc log with new fields decodes without error.

use loupe_core::ac_key::Digest;

use crate::error::CasError;
use crate::proto::{ActionResult, decode_action_result, decode_digest};

/// One decoded gRPC call entry. We surface only the call shape that
/// matters for AC-key comparison; other RPC kinds (Execute, Read,
/// Write, FindMissingBlobs, ...) decode to [`Entry::Other`] carrying
/// the method name, so a caller can still see what Bazel asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// `ActionCache.GetActionResult(action_digest)` — Bazel asked the
    /// cache whether it had a result. The action digest in the
    /// request is the Bazel-computed REAPI Action key for the spawn.
    GetActionResult {
        method_name: String,
        action_digest: Digest,
        /// `Some(_)` if the call returned hit data, `None` on miss
        /// (Bazel logs the miss as an entry with no `response` set
        /// or with `status.code != OK`).
        response: Option<ActionResult>,
    },
    /// `ActionCache.UpdateActionResult(action_digest, action_result)`
    /// — Bazel pushed a freshly-executed result into the cache. The
    /// `action_result.output_files[i].path` entries plus the
    /// `action_digest` give a `paths → digest` mapping that's the
    /// strongest join key for matching against Loupe's actions.
    UpdateActionResult {
        method_name: String,
        action_digest: Digest,
        action_result: ActionResult,
    },
    /// Any other RPC. Carries the method name for diagnostic
    /// reporting; we don't decode the body.
    Other { method_name: String },
}

/// Read every `LogEntry` from a Bazel grpc log byte stream. Entries
/// are length-delimited (varint length + body); the loop bottoms out
/// at EOF.
pub fn decode_grpc_log(mut bytes: &[u8]) -> Result<Vec<Entry>, CasError> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        let (len, rest) = read_varint(bytes)?;
        let len = len as usize;
        if rest.len() < len {
            return Err(CasError::Decode(format!(
                "grpc log entry truncated: need {len} bytes, have {}",
                rest.len()
            )));
        }
        let (entry_bytes, after) = rest.split_at(len);
        out.push(decode_log_entry(entry_bytes)?);
        bytes = after;
    }
    Ok(out)
}

/// Decode one `LogEntry` body (no outer length prefix).
fn decode_log_entry(bytes: &[u8]) -> Result<Entry, CasError> {
    let mut r = Reader::new(bytes);
    let mut method_name = String::new();
    let mut details_bytes: Option<&[u8]> = None;
    while !r.eof() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            // metadata = 1, status = 2 — skip.
            (3, 2) => {
                method_name = std::str::from_utf8(r.len_delim()?)
                    .map_err(|e| CasError::Decode(format!("method_name not UTF-8: {e}")))?
                    .to_string();
            }
            (4, 2) => {
                details_bytes = Some(r.len_delim()?);
            }
            // start_time = 5, end_time = 6 — skip.
            (_, w) => r.skip(w)?,
        }
    }
    let Some(details) = details_bytes else {
        // Calls with no body — unusual but legal; bubble up as Other.
        return Ok(Entry::Other { method_name });
    };
    decode_rpc_call_details(&method_name, details)
}

/// Dispatch on the `RpcCallDetails` oneof. Tag numbers per Bazel 9's
/// `remote_execution_log.proto`:
///   get_action_result = 8, update_action_result = 13.
/// Other RPC kinds collapse into `Entry::Other`.
fn decode_rpc_call_details(method_name: &str, bytes: &[u8]) -> Result<Entry, CasError> {
    let mut r = Reader::new(bytes);
    while !r.eof() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (8, 2) => return decode_get_action_result_details(method_name, r.len_delim()?),
            (13, 2) => return decode_update_action_result_details(method_name, r.len_delim()?),
            (_, w) => r.skip(w)?,
        }
    }
    Ok(Entry::Other {
        method_name: method_name.to_string(),
    })
}

fn decode_get_action_result_details(method_name: &str, bytes: &[u8]) -> Result<Entry, CasError> {
    let mut r = Reader::new(bytes);
    let mut action_digest: Option<Digest> = None;
    let mut response: Option<ActionResult> = None;
    while !r.eof() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            // request = 1: GetActionResultRequest
            (1, 2) => {
                let req = r.len_delim()?;
                action_digest = decode_get_action_result_request_for_digest(req)?;
            }
            // response = 2: ActionResult
            (2, 2) => {
                response = Some(decode_action_result(r.len_delim()?)?);
            }
            (_, w) => r.skip(w)?,
        }
    }
    let action_digest = action_digest.ok_or_else(|| {
        CasError::Decode("GetActionResultDetails has no request.action_digest".to_string())
    })?;
    Ok(Entry::GetActionResult {
        method_name: method_name.to_string(),
        action_digest,
        response,
    })
}

fn decode_update_action_result_details(method_name: &str, bytes: &[u8]) -> Result<Entry, CasError> {
    let mut r = Reader::new(bytes);
    let mut action_digest: Option<Digest> = None;
    let mut action_result: Option<ActionResult> = None;
    while !r.eof() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            // request = 1: UpdateActionResultRequest
            (1, 2) => {
                let req_bytes = r.len_delim()?;
                let (d, ar) = decode_update_action_result_request(req_bytes)?;
                action_digest = d;
                action_result = ar;
            }
            // response = 2: ActionResult (server-echoed; mirrors request.action_result).
            (2, 2) => {
                if action_result.is_none() {
                    action_result = Some(decode_action_result(r.len_delim()?)?);
                } else {
                    let _ = r.len_delim()?;
                }
            }
            (_, w) => r.skip(w)?,
        }
    }
    let action_digest = action_digest.ok_or_else(|| {
        CasError::Decode("UpdateActionResultDetails has no request.action_digest".to_string())
    })?;
    let action_result = action_result.ok_or_else(|| {
        CasError::Decode("UpdateActionResultDetails has no action_result".to_string())
    })?;
    Ok(Entry::UpdateActionResult {
        method_name: method_name.to_string(),
        action_digest,
        action_result,
    })
}

/// `GetActionResultRequest { instance_name: 1, action_digest: 2, ... }`.
/// Surface only the digest.
fn decode_get_action_result_request_for_digest(bytes: &[u8]) -> Result<Option<Digest>, CasError> {
    let mut r = Reader::new(bytes);
    let mut action_digest: Option<Digest> = None;
    while !r.eof() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (2, 2) => action_digest = Some(decode_digest(r.len_delim()?)?),
            (_, w) => r.skip(w)?,
        }
    }
    Ok(action_digest)
}

/// `UpdateActionResultRequest { instance_name: 1, action_digest: 2,
/// action_result: 3, ... }`. Surface digest + result.
fn decode_update_action_result_request(
    bytes: &[u8],
) -> Result<(Option<Digest>, Option<ActionResult>), CasError> {
    let mut r = Reader::new(bytes);
    let mut action_digest: Option<Digest> = None;
    let mut action_result: Option<ActionResult> = None;
    while !r.eof() {
        let (field, wire) = r.tag()?;
        match (field, wire) {
            (2, 2) => action_digest = Some(decode_digest(r.len_delim()?)?),
            (3, 2) => action_result = Some(decode_action_result(r.len_delim()?)?),
            (_, w) => r.skip(w)?,
        }
    }
    Ok((action_digest, action_result))
}

// ---- wire-format reader primitives ----

fn read_varint(bytes: &[u8]) -> Result<(u64, &[u8]), CasError> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    let mut i = 0;
    loop {
        if i >= bytes.len() {
            return Err(CasError::Decode(
                "grpc log: truncated framing varint".to_string(),
            ));
        }
        let byte = bytes[i];
        i += 1;
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok((result, &bytes[i..]));
        }
        shift += 7;
        if shift >= 64 {
            return Err(CasError::Decode(
                "grpc log: framing varint > 64 bits".to_string(),
            ));
        }
    }
}

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
        let (v, rest) = read_varint(&self.buf[self.pos..])?;
        self.pos = self.buf.len() - rest.len();
        Ok(v)
    }

    fn tag(&mut self) -> Result<(u32, u8), CasError> {
        let v = self.varint()?;
        Ok(((v >> 3) as u32, (v & 0x7) as u8))
    }

    fn len_delim(&mut self) -> Result<&'a [u8], CasError> {
        let len = self.varint()? as usize;
        if self.pos + len > self.buf.len() {
            return Err(CasError::Decode(format!(
                "grpc log: length-delimited overrun: need {len}, have {}",
                self.buf.len() - self.pos
            )));
        }
        let out = &self.buf[self.pos..self.pos + len];
        self.pos += len;
        Ok(out)
    }

    fn skip(&mut self, wire: u8) -> Result<(), CasError> {
        match wire {
            0 => {
                self.varint()?;
            }
            1 => {
                if self.pos + 8 > self.buf.len() {
                    return Err(CasError::Decode("grpc log: truncated fixed64".to_string()));
                }
                self.pos += 8;
            }
            2 => {
                self.len_delim()?;
            }
            5 => {
                if self.pos + 4 > self.buf.len() {
                    return Err(CasError::Decode("grpc log: truncated fixed32".to_string()));
                }
                self.pos += 4;
            }
            _ => {
                return Err(CasError::Decode(format!(
                    "grpc log: unsupported wire type {wire}"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::OutputFile;

    // Hand-rolled encoders for the LogEntry / RpcCallDetails wrapper
    // so we can round-trip a fixture and verify the decoder reads
    // back what we encoded.

    fn varint_w(buf: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            buf.push(((v & 0x7f) as u8) | 0x80);
            v >>= 7;
        }
        buf.push(v as u8);
    }

    fn tag_w(buf: &mut Vec<u8>, field: u32, wire: u8) {
        varint_w(buf, ((field as u64) << 3) | (wire as u64));
    }

    fn str_w(buf: &mut Vec<u8>, field: u32, s: &str) {
        tag_w(buf, field, 2);
        varint_w(buf, s.len() as u64);
        buf.extend_from_slice(s.as_bytes());
    }

    fn submsg_w(buf: &mut Vec<u8>, field: u32, body: &[u8]) {
        tag_w(buf, field, 2);
        varint_w(buf, body.len() as u64);
        buf.extend_from_slice(body);
    }

    fn encode_digest(d: &Digest) -> Vec<u8> {
        let mut out = Vec::new();
        if !d.hash.is_empty() {
            str_w(&mut out, 1, &d.hash);
        }
        if d.size_bytes != 0 {
            tag_w(&mut out, 2, 0);
            varint_w(&mut out, d.size_bytes as u64);
        }
        out
    }

    fn encode_get_action_result_request(digest: &Digest) -> Vec<u8> {
        let mut out = Vec::new();
        submsg_w(&mut out, 2, &encode_digest(digest));
        out
    }

    fn encode_get_action_result_details(digest: &Digest) -> Vec<u8> {
        let mut out = Vec::new();
        submsg_w(&mut out, 1, &encode_get_action_result_request(digest));
        out
    }

    fn encode_output_file(f: &OutputFile) -> Vec<u8> {
        let mut out = Vec::new();
        str_w(&mut out, 1, &f.path);
        submsg_w(&mut out, 2, &encode_digest(&f.digest));
        if f.is_executable {
            tag_w(&mut out, 4, 0);
            varint_w(&mut out, 1);
        }
        out
    }

    fn encode_action_result(ar: &ActionResult) -> Vec<u8> {
        let mut out = Vec::new();
        for of in &ar.output_files {
            submsg_w(&mut out, 2, &encode_output_file(of));
        }
        out
    }

    fn encode_update_action_result_request(digest: &Digest, ar: &ActionResult) -> Vec<u8> {
        let mut out = Vec::new();
        submsg_w(&mut out, 2, &encode_digest(digest));
        submsg_w(&mut out, 3, &encode_action_result(ar));
        out
    }

    fn encode_update_action_result_details(digest: &Digest, ar: &ActionResult) -> Vec<u8> {
        let mut out = Vec::new();
        submsg_w(
            &mut out,
            1,
            &encode_update_action_result_request(digest, ar),
        );
        out
    }

    fn encode_log_entry(method: &str, details_tag: u32, details_body: &[u8]) -> Vec<u8> {
        let mut details = Vec::new();
        submsg_w(&mut details, details_tag, details_body);
        let mut out = Vec::new();
        str_w(&mut out, 3, method);
        submsg_w(&mut out, 4, &details);
        out
    }

    fn frame(entry: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        varint_w(&mut out, entry.len() as u64);
        out.extend_from_slice(entry);
        out
    }

    #[test]
    fn decodes_single_get_action_result_entry() {
        let d = Digest {
            hash: "deadbeef".repeat(8),
            size_bytes: 144,
        };
        let body = encode_log_entry(
            "/build.bazel.remote.execution.v2.ActionCache/GetActionResult",
            8,
            &encode_get_action_result_details(&d),
        );
        let log = frame(&body);
        let entries = decode_grpc_log(&log).unwrap();
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            Entry::GetActionResult {
                method_name,
                action_digest,
                response,
            } => {
                assert!(method_name.contains("GetActionResult"));
                assert_eq!(action_digest, &d);
                assert!(response.is_none());
            }
            other => panic!("expected GetActionResult, got {other:?}"),
        }
    }

    #[test]
    fn decodes_update_action_result_carrying_output_paths() {
        let action_digest = Digest::of_bytes(b"some-action");
        let ar = ActionResult {
            output_files: vec![OutputFile {
                path: "bazel-out/k8-fastbuild/bin/pkg/out.bin".to_string(),
                digest: Digest::of_bytes(b"content"),
                is_executable: false,
            }],
            output_directories: vec![],
            exit_code: 0,
            stdout_digest: None,
            stderr_digest: None,
        };
        let body = encode_log_entry(
            "/build.bazel.remote.execution.v2.ActionCache/UpdateActionResult",
            13,
            &encode_update_action_result_details(&action_digest, &ar),
        );
        let entries = decode_grpc_log(&frame(&body)).unwrap();
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            Entry::UpdateActionResult {
                action_digest: d,
                action_result,
                ..
            } => {
                assert_eq!(d, &action_digest);
                assert_eq!(action_result, &ar);
            }
            other => panic!("expected UpdateActionResult, got {other:?}"),
        }
    }

    #[test]
    fn unknown_rpc_kind_decodes_to_other_with_method_name() {
        // Construct a LogEntry whose method is some other RPC and
        // whose RpcCallDetails has no recognised oneof set.
        let mut entry = Vec::new();
        str_w(&mut entry, 3, "/some/other/Method");
        // Empty details body — RpcCallDetails with no oneof set.
        submsg_w(&mut entry, 4, &[]);
        let entries = decode_grpc_log(&frame(&entry)).unwrap();
        assert_eq!(entries.len(), 1);
        match &entries[0] {
            Entry::Other { method_name } => assert_eq!(method_name, "/some/other/Method"),
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn stream_of_three_entries_decodes_in_order() {
        let d1 = Digest::of_bytes(b"action-1");
        let d2 = Digest::of_bytes(b"action-2");
        let e1 = frame(&encode_log_entry(
            "GetActionResult",
            8,
            &encode_get_action_result_details(&d1),
        ));
        let mut other_entry = Vec::new();
        str_w(&mut other_entry, 3, "/Capabilities");
        submsg_w(&mut other_entry, 4, &[]);
        let e2 = frame(&other_entry);
        let e3 = frame(&encode_log_entry(
            "GetActionResult",
            8,
            &encode_get_action_result_details(&d2),
        ));
        let stream = [e1, e2, e3].concat();
        let entries = decode_grpc_log(&stream).unwrap();
        assert_eq!(entries.len(), 3);
        match &entries[0] {
            Entry::GetActionResult { action_digest, .. } => assert_eq!(action_digest, &d1),
            _ => panic!(),
        }
        assert!(matches!(&entries[1], Entry::Other { .. }));
        match &entries[2] {
            Entry::GetActionResult { action_digest, .. } => assert_eq!(action_digest, &d2),
            _ => panic!(),
        }
    }

    #[test]
    fn truncated_framing_is_decode_error() {
        let bytes = vec![0xff, 0xff]; // varint with high bits set, no continuation
        assert!(matches!(decode_grpc_log(&bytes), Err(CasError::Decode(_))));
    }
}
