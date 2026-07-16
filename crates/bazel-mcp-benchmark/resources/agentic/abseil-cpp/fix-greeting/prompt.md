The greeting formatter has regressed. It must trim leading and trailing ASCII
whitespace from the supplied name before returning `Hello, <name>!`. If the
trimmed name is empty, it must return `Hello, world!`.

Implement the fix in the formatter source without editing the test source or
changing the public function signature. Run the relevant Bazel tests before
finishing.
