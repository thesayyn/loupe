def main(targets):
    out = []
    for t in targets:
        out.append({
            "label": str(t.label),
            "mnemonics": [a.mnemonic for a in t.actions],
            "files": [f.path for f in t.files.to_list()],
        })
    return out
