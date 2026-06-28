def main(targets):
    return [{"label": str(t.label), "mnemonics": [a.mnemonic for a in t.actions], "files": [f.path for f in t.files.to_list()]} for t in targets]
