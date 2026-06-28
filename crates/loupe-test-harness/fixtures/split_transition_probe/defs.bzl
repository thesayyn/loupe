def _split_impl(settings, attr):
    return {
        "arm": {"//:cpu_flag": "arm64"},
        "x86": {"//:cpu_flag": "x86_64"},
    }

split_cpu = transition(implementation = _split_impl, inputs = [], outputs = ["//:cpu_flag"])

def _leaf_impl(ctx):
    out = ctx.actions.declare_file(ctx.attr.tag)
    ctx.actions.write(out, "")
    return [DefaultInfo(files = depset([out]))]

leaf = rule(implementation = _leaf_impl, attrs = {"tag": attr.string()})

def _collector_impl(ctx):
    outs = []
    for d in ctx.attr.dep:
        outs += d[DefaultInfo].files.to_list()
    return [DefaultInfo(files = depset(outs))]

collector = rule(implementation = _collector_impl, attrs = {"dep": attr.label(cfg = split_cpu)})
