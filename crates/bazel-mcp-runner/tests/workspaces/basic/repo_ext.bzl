def _broken_repository_impl(repository_ctx):
    fail("MATRIX_EXTERNAL_REPOSITORY_ROOT_CAUSE")

broken_repository = repository_rule(implementation = _broken_repository_impl)

def _broken_extension_impl(module_ctx):
    broken_repository(name = "matrix_broken_repo")

broken_extension = module_extension(implementation = _broken_extension_impl)
