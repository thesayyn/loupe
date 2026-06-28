def _to_arm(settings, attr):
    return {"//command_line_option:cpu": "arm64"}

arm_transition = transition(
    implementation = _to_arm,
    inputs = [],
    outputs = ["//command_line_option:cpu"],
)

def _impl(ctx):
    out = ctx.actions.declare_file("o.txt")
    ctx.actions.write(out, "")
    return [DefaultInfo(files = depset([out]))]

armed = rule(implementation = _impl, cfg = arm_transition)
plain = rule(implementation = _impl)
