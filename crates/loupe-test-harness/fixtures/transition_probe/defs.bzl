def _to_prod(settings, attr):
    return {"//:flavor": "prod"}

to_prod = transition(implementation = _to_prod, inputs = [], outputs = ["//:flavor"])

def _printer_impl(ctx):
    out = ctx.actions.declare_file(ctx.attr.suffix)
    ctx.actions.write(out, "")
    return [DefaultInfo(files = depset([out]))]

printer = rule(
    implementation = _printer_impl,
    cfg = to_prod,
    attrs = {"suffix": attr.string()},
)

no_transition = rule(
    implementation = _printer_impl,
    attrs = {"suffix": attr.string()},
)
