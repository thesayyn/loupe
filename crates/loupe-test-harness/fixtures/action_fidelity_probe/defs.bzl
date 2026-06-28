def _symlink_impl(ctx):
    out = ctx.actions.declare_file("link.txt")
    src = ctx.actions.declare_file("src.txt")
    ctx.actions.write(src, "x")
    ctx.actions.symlink(output = out, target_file = src)
    return [DefaultInfo(files = depset([out]))]

symlinker = rule(implementation = _symlink_impl)

def _runsh_impl(ctx):
    out = ctx.actions.declare_file("rs.txt")
    ctx.actions.run_shell(outputs = [out], command = "touch $@")
    return [DefaultInfo(files = depset([out]))]

runsh = rule(implementation = _runsh_impl)

def _dir_impl(ctx):
    d = ctx.actions.declare_directory("treedir")
    ctx.actions.run_shell(outputs = [d], command = "mkdir -p $@", mnemonic = "MakeDir")
    return [DefaultInfo(files = depset([d]))]

dirmaker = rule(implementation = _dir_impl)
