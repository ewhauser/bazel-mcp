def _invoice_impl(ctx):
    fail("invoice rule rejected missing tax region for %s" % ctx.label)

invoice_rule = rule(implementation = _invoice_impl)
