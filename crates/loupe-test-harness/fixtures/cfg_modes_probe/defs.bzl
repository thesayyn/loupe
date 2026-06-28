def _leaf_impl(ctx):
    out = ctx.actions.declare_file(ctx.label.name + ".txt")
    ctx.actions.write(out, "")
    return [DefaultInfo(files = depset([out]))]

leaf = rule(implementation = _leaf_impl)

def _parent_impl(ctx):
    outs = []
    for d in [ctx.attr.t, ctx.attr.e, ctx.attr.n, ctx.attr.u]:
        outs += d.files.to_list()
    return [DefaultInfo(files = depset(outs))]

parent = rule(implementation = _parent_impl, attrs = {
    "t": attr.label(cfg = "target"),
    "e": attr.label(cfg = "exec"),
    "n": attr.label(cfg = config.none()),
    "u": attr.label(cfg = config.unconfigured()),
})
