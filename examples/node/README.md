# TypeScript, SWC, and JavaScript reducer examples

This dependency island pins Aspect `rules_js`, a hermetic Node 24.12.0
toolchain, the TypeScript 5.9.3 compiler archive, and SWC's platform-neutral
WebAssembly package at 1.15.43. `//:success` is a working JavaScript binary.
The manual cases run real `tsc`, SWC, and Node processes for TypeScript and SWC
compilation, JavaScript syntax, and JavaScript runtime failures.
