HelloInfo = provider(
    doc = "Trivial provider used by Loupe fixture #1.",
    fields = {"greeting": "Greeting string."},
)

def _hello_impl(ctx):
    return [HelloInfo(greeting = ctx.attr.greeting)]

hello = rule(
    implementation = _hello_impl,
    attrs = {
        "greeting": attr.string(default = "hello"),
    },
)
