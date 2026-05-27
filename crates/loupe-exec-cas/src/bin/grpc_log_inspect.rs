//! Read a Bazel `--remote_grpc_log` file and dump every recorded
//! REAPI Action digest Bazel computed.
//!
//! Pair with `cargo run -p loupe-exec-cas --bin grpc_log_inspect --
//! /path/to/grpc.log`. The output lists each `GetActionResult` /
//! `UpdateActionResult` call with the digest Bazel asked the cache
//! about; for `UpdateActionResult` we also show the output paths,
//! which is the strongest join key for matching against Loupe's
//! recorded actions.

use std::process::ExitCode;

use loupe_exec_cas::{GrpcLogEntry, decode_grpc_log};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: grpc_log_inspect <grpc-log-file>");
        return ExitCode::from(2);
    }
    let bytes = match std::fs::read(&args[1]) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("read {}: {e}", args[1]);
            return ExitCode::from(1);
        }
    };
    let entries = match decode_grpc_log(&bytes) {
        Ok(es) => es,
        Err(e) => {
            eprintln!("decode: {e}");
            return ExitCode::from(1);
        }
    };

    let mut n_get = 0usize;
    let mut n_update = 0usize;
    let mut n_other = 0usize;
    for entry in &entries {
        match entry {
            GrpcLogEntry::GetActionResult {
                method_name,
                action_digest,
                response,
            } => {
                n_get += 1;
                let outcome = match response {
                    Some(_) => "HIT",
                    None => "MISS",
                };
                println!(
                    "[{outcome}] GetActionResult digest={}/{} method={method_name}",
                    action_digest.hash, action_digest.size_bytes,
                );
            }
            GrpcLogEntry::UpdateActionResult {
                action_digest,
                action_result,
                ..
            } => {
                n_update += 1;
                println!(
                    "[UPLOAD] UpdateActionResult digest={}/{}",
                    action_digest.hash, action_digest.size_bytes,
                );
                for of in &action_result.output_files {
                    println!(
                        "    output: {} digest={}/{} exec={}",
                        of.path, of.digest.hash, of.digest.size_bytes, of.is_executable
                    );
                }
                for od in &action_result.output_directories {
                    println!(
                        "    tree:   {} tree_digest={}/{}",
                        od.path, od.tree_digest.hash, od.tree_digest.size_bytes,
                    );
                }
            }
            GrpcLogEntry::Other { method_name } => {
                n_other += 1;
                if std::env::var("VERBOSE").is_ok() {
                    println!("[other] {method_name}");
                }
            }
        }
    }

    eprintln!(
        "\n{} entries: {n_get} GetActionResult, {n_update} UpdateActionResult, {n_other} other",
        entries.len()
    );
    ExitCode::SUCCESS
}
