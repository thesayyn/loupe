def _impl(ctx):
    # Root-package main-repo target: bin-relative path is just the name.
    out = ctx.actions.declare_shareable_artifact("gen.txt")
    ctx.actions.write(out, "")
    return [DefaultInfo(files = depset([out]))]

dsa = rule(implementation = _impl)
