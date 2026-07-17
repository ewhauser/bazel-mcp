# C++ reducer examples

This workspace provides successful and intentionally broken C++ targets using
`rules_cc` and GoogleTest. The reducer harness exercises:

- Clang/GCC source diagnostics;
- missing headers;
- undefined symbols and platform linker wrappers;
- located GoogleTest assertion failures;
- GoogleTest C++ exception failures.

The examples were reduced from a manual exercise against
[`google/googletest`](https://github.com/google/googletest) commit
`a25f43576effe4ebe887412a603cddfa8fc3ba64`.
