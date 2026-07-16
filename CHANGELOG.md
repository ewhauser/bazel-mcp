# Changelog

## [0.3.0](https://github.com/ewhauser/bazel-mcp/compare/v0.2.0...v0.3.0) (2026-07-16)


### Features

* **server:** allow smaller inspection budgets ([7695d8a](https://github.com/ewhauser/bazel-mcp/commit/7695d8af4b3c5f58fd78eb9abb6c53591ba7c101))
* **server:** expose invocation performance metrics ([9f61b81](https://github.com/ewhauser/bazel-mcp/commit/9f61b81fab770ac03ff4388404cb39f7e83e439d))
* **server:** expose retained invocation ledger ([6626127](https://github.com/ewhauser/bazel-mcp/commit/6626127a4c9a6e1af3a79a3c6a0bab866316fe20))
* **server:** filter invocation ledger by command ([7b910aa](https://github.com/ewhauser/bazel-mcp/commit/7b910aad31744626fc4b20b8f1a4f8270f2babad))
* **server:** filter invocation ledger by state ([e9c6efd](https://github.com/ewhauser/bazel-mcp/commit/e9c6efd9281efb13f9a8050e361f258ee2acb0ed))
* **server:** make TOON the default result encoding ([2a99927](https://github.com/ewhauser/bazel-mcp/commit/2a999275deb3803d9015c3685787314252234859))


### Bug Fixes

* **bazel:** declare process test BEP dependency ([c49b356](https://github.com/ewhauser/bazel-mcp/commit/c49b356457f2d7f3b761e648c4102f9e9e937f60))
* **ci:** pin conformance result encoding ([b83be1e](https://github.com/ewhauser/bazel-mcp/commit/b83be1e93dbcac64aa636bff0c1f34f0e7df0db3))
* **release:** publish artifacts from release please ([4477795](https://github.com/ewhauser/bazel-mcp/commit/447779570a6c7d4a7dd8e15fd7f605b8f66ceb1f))
* **server:** return compact invocation ledger rows ([59df058](https://github.com/ewhauser/bazel-mcp/commit/59df058a68bedf634b210fec7876d234ea9ff2b3))
* **store:** flush telemetry on server shutdown ([199ad3a](https://github.com/ewhauser/bazel-mcp/commit/199ad3a94162320e4d126210a104dfd5a5d75027))
* **store:** make retention cutoff inclusive ([cfcbe0a](https://github.com/ewhauser/bazel-mcp/commit/cfcbe0a30017e16c7a2643fb9b37b64f4606a397))


### Performance Improvements

* **benchmark:** add paired MCP inspect harness ([32a0663](https://github.com/ewhauser/bazel-mcp/commit/32a06634579384ed8550d4fcc318fb3b26a62c34))
* **server:** serialize tool results once ([c40715f](https://github.com/ewhauser/bazel-mcp/commit/c40715f89268287d53c476ee5179d501c6c01132))

## [0.2.0](https://github.com/ewhauser/bazel-mcp/compare/v0.1.0...v0.2.0) (2026-07-16)


### Features

* add negotiated MCP task execution ([33811a0](https://github.com/ewhauser/bazel-mcp/commit/33811a0c339b66a2ebf00c9c0db19f252326f9ac))
* **benchmark:** add agentic Bazel comparison ([c26880a](https://github.com/ewhauser/bazel-mcp/commit/c26880a0f817a556439bbc88cc8caff4d29794f4))
* replace database with filesystem storage ([43d7fb4](https://github.com/ewhauser/bazel-mcp/commit/43d7fb49f628714a43701e48445fb3f5037fd18b))


### Bug Fixes

* **build:** restore cross-platform builds ([7bfdb46](https://github.com/ewhauser/bazel-mcp/commit/7bfdb463283878f260527041cde945c049744af7))
* **release:** support virtual cargo workspace ([23950c8](https://github.com/ewhauser/bazel-mcp/commit/23950c829c3795ffdf3cd1403de31b29f8fc7405))
* **release:** support virtual Cargo workspace ([cb2de32](https://github.com/ewhauser/bazel-mcp/commit/cb2de32e1603bdc08e8b044957dc67b6e86c2aeb))


### Performance Improvements

* optimize filesystem storage pipeline ([d0fcad9](https://github.com/ewhauser/bazel-mcp/commit/d0fcad9960fc046d4256addbd23ec9ed65de2a36))
* record optimized storage benchmarks ([f1d5ca3](https://github.com/ewhauser/bazel-mcp/commit/f1d5ca3fc8d4d42cb83024517acceeddeb9243fa))
* replace database storage with optimized filesystem pipeline ([c518229](https://github.com/ewhauser/bazel-mcp/commit/c5182295ccf312a2dabd3c0116dbb5d83e12ae35))

## Changelog

All notable changes to this project will be documented in this file.
