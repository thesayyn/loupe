"""Example Loupe cquery script for the rules_js probe.

Run with:
    loupe cquery \\
        crates/loupe-test-harness/fixtures/rules_js_examples_probe \\
        --script crates/loupe-test-harness/fixtures/\\
rules_js_examples_probe/query.bzl

Demonstrates: per-target label + executable + DefaultInfo file
extensions, plus a workspace-wide tally of distinct file extensions
seen across every analyzed target's `DefaultInfo.files`.
"""

def _ext(path):
    dot = path.rfind(".")
    return path[dot:] if dot >= 0 else ""

def main(targets):
    per_target = []
    for t in targets:
        for a in t.actions:
            for i in a.inputs:
                print(i.owner)
    return {}
