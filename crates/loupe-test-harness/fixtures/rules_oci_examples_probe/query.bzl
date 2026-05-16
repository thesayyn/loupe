"""Example Loupe cquery script for the rules_oci probe.

Run with:
    loupe cquery \\
        crates/loupe-test-harness/fixtures/rules_oci_examples_probe \\
        --script crates/loupe-test-harness/fixtures/\\
rules_oci_examples_probe/query.bzl

`targets` is the full list of analyzed `Target` values, sorted by
canonical label. The Target surface exposes `.label`, `.files`,
`.actions[i].{mnemonic, inputs, outputs, argv, env, content}`,
`.files_to_run`, `.output_groups`, and `[Provider]` indexing.
"""

def main(targets):
    per_target = [
        {
            "label": str(t.label),
            "mnemonics": [a.mnemonic for a in t.actions],
        }
        for t in targets
    ]

    tally = {}
    for t in targets:
        for a in t.actions:
            tally[a.mnemonic] = tally.get(a.mnemonic, 0) + 1

    return {
        "per_target": per_target,
        "mnemonic_tally": tally,
    }
