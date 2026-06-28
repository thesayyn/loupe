def main(targets):
    return [{"label": str(t.label), "files": [f.path for f in t.files.to_list()]} for t in targets]
