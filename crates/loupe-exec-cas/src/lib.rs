//! Remote-cache hydration for Loupe's REAPI v2 Action keys.
//!
//! Pairs with `loupe_core::ac_key`: where `ac_key` computes the
//! Bazel-identical Action digest given resolved inputs,
//! `loupe-exec-cas` supplies the missing piece — looking up an
//! upstream `Run`/`RunShell` output's digest in a REAPI v2 remote
//! cache so a downstream action's input merkle tree can be built
//! without executing anything.
//!
//! DICE-native by design: every digest, every `ActionResult` fetch,
//! every source-file hash is a `dice::Key`. Parallelism, caching,
//! and incrementality fall out of DICE's evaluator. See [`keys`].

pub mod client;
pub mod error;
pub mod global_index;
pub mod grpc_log;
pub mod keys;
pub mod proto;

pub use client::{BazelRemoteHttpClient, CacheClient, StubCacheClient};
pub use error::CasError;
pub use global_index::{GlobalActionIndexKey, ProducerEntry, ProducerLookup};
pub use grpc_log::{Entry as GrpcLogEntry, decode_grpc_log};
pub use keys::{
    ActionDigestKey, ActionRef, ActionResultKey, HasCacheClient, OutputDigestKey, ResolvedInput,
    SetCacheClient, SourceFileDigestKey,
};
pub use proto::{
    ActionResult, Directory, DirectoryNode, FileNode, OutputDirectory, OutputFile, SymlinkNode,
    Tree, decode_tree, tree_lookup,
};
