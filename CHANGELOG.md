# Changelog

## [0.5.0](https://github.com/ewhauser/bazel-mcp/compare/v0.4.0...v0.5.0) (2026-07-17)


### Features

* **reducer:** parse SWC diagnostics ([3ba358c](https://github.com/ewhauser/bazel-mcp/commit/3ba358cebc7c2e5377d7ec0e789474e3e6f6ad95))
* **testing:** add reducer integration framework ([c24bea1](https://github.com/ewhauser/bazel-mcp/commit/c24bea166193dc084a30faa39ba53747b22d8249))


### Bug Fixes

* **ci:** stabilize reducer integration checks ([e7d3628](https://github.com/ewhauser/bazel-mcp/commit/e7d36280bc219b0ef846d473a339e499c5af7429))
* **reducer:** normalize recorded workspace locations ([87bd8c1](https://github.com/ewhauser/bazel-mcp/commit/87bd8c1a2043f13b6095d4d098266ab0c8694a71))
* **reducer:** select located SWC source line ([ae8a823](https://github.com/ewhauser/bazel-mcp/commit/ae8a8234cd628feb27e5684a921c5e018fcc5f01))
* **runner:** coordinate shared output bases ([e57aaa9](https://github.com/ewhauser/bazel-mcp/commit/e57aaa9cac9c0255aa7e47125f58f9e2b9087b70))
* **runner:** measure silent native lock waits ([0176013](https://github.com/ewhauser/bazel-mcp/commit/017601346a62231f368b561ae6971c08670fd127))
* **store:** support shared multiprocess cache ([cc465ad](https://github.com/ewhauser/bazel-mcp/commit/cc465ad99b117f20473b804917f93795d43a4df0))


### Performance Improvements

* reduce filtered query allocations ([a31a45a](https://github.com/ewhauser/bazel-mcp/commit/a31a45a4537d18432dc9cceeed58fe3746ce244f))
* reduce query and reducer allocations ([7e2a305](https://github.com/ewhauser/bazel-mcp/commit/7e2a305be0e2ce6fb1a43a890a6f9feae76596a5))

## [0.4.0](https://github.com/ewhauser/bazel-mcp/compare/v0.3.0...v0.4.0) (2026-07-17)


### ⚠ BREAKING CHANGES

* **bazel:** drop Bazel 7 support

### Features

* **bazel:** drop Bazel 7 support ([845ff78](https://github.com/ewhauser/bazel-mcp/commit/845ff78e243b1a78f094fb1a7f9f9153b8785874))
* **bes:** add loopback build event service ([03cc670](https://github.com/ewhauser/bazel-mcp/commit/03cc67042ec2d0e55e39612a523feec861ea365a))
* **reducer:** add extensible Starlark reducers ([6879851](https://github.com/ewhauser/bazel-mcp/commit/68798514b0ad6f16c49d2f716b82f85f4bc343b0))
* **reducer:** add extensible Starlark reducers ([334e899](https://github.com/ewhauser/bazel-mcp/commit/334e8998892d1688760922024775903d991c12cc))
* **release:** publish Windows binaries ([a672534](https://github.com/ewhauser/bazel-mcp/commit/a672534437c2719bb25f3b54498698b117eb9d17))


### Bug Fixes

* **ci:** accept Starlark dependencies and publish docs ([9d9c8b9](https://github.com/ewhauser/bazel-mcp/commit/9d9c8b9f3f1265490eca39dea4cba18f35a966f3))
* **ci:** isolate remote cache smoke test ([70f2795](https://github.com/ewhauser/bazel-mcp/commit/70f2795affa7f706a20a0a91fe5b3816d5c22bae))
* **docs:** repair wide landing layout ([9d3f59c](https://github.com/ewhauser/bazel-mcp/commit/9d3f59ccbe07246510b9f48a8fccd226f88ed24b))
* **docs:** repair wide landing layout ([0a6cc2e](https://github.com/ewhauser/bazel-mcp/commit/0a6cc2ef085dba4f427fdbe9a08d971e6ddd4a3f))
* **reducer:** preserve Go failure diagnostics ([323c972](https://github.com/ewhauser/bazel-mcp/commit/323c972df442d4527cc61f6e93dd4d2c0f58a10b))
* **reducer:** preserve Go failure diagnostics ([f9fb822](https://github.com/ewhauser/bazel-mcp/commit/f9fb8228b41b7d3464f1648b294f6543b3d92741))
* **reducer:** preserve Python traceback diagnostics ([1442817](https://github.com/ewhauser/bazel-mcp/commit/1442817bef3da1e9c5e5a870335aaadbd87233ce))
* **reducer:** preserve Python traceback diagnostics ([f030f80](https://github.com/ewhauser/bazel-mcp/commit/f030f8019da42c31bf44c15571224191226ddc49))
* **reducer:** surface C++ root causes ([b3cb7dd](https://github.com/ewhauser/bazel-mcp/commit/b3cb7dd956a2d8c0800698eee73cc0a8a0e62db1))
* **reducer:** surface C++ root causes ([55e75a2](https://github.com/ewhauser/bazel-mcp/commit/55e75a29cfc2b0bc26205748bf5884e196ec5ede))
* **reducer:** surface Java root causes ([88a615d](https://github.com/ewhauser/bazel-mcp/commit/88a615d60b65ecda070a3fb9c9c04ac554df524b))
* **reducer:** surface Java root causes ([e349286](https://github.com/ewhauser/bazel-mcp/commit/e3492869a0bf522f7078af40c35962649afe58e9))
* **reducer:** surface protobuf diagnostics ([aff28a0](https://github.com/ewhauser/bazel-mcp/commit/aff28a074a65233ffd8af94f25042100d0381982))
* **reducer:** surface protobuf diagnostics ([c076157](https://github.com/ewhauser/bazel-mcp/commit/c076157f4a7ba9ef5fc9b2b59e49b0ed12fe99e0))
* **reducer:** surface Starlark root causes ([ca14985](https://github.com/ewhauser/bazel-mcp/commit/ca1498532a428efc394e2ab1d5056e3a15123f05))
* **reducer:** surface Starlark root causes ([8e52df4](https://github.com/ewhauser/bazel-mcp/commit/8e52df429f4ac1682233e4b6ff6b4c4e51eaf834))
* **reducer:** surface TypeScript and JavaScript diagnostics ([109e183](https://github.com/ewhauser/bazel-mcp/commit/109e18318375acdd7cf0a11978099d34b458ceb2))
* **reducer:** surface TypeScript and JavaScript diagnostics ([842c658](https://github.com/ewhauser/bazel-mcp/commit/842c658bbde08be46d931e96f0b234f1e4874536))
* **release:** publish assets through REST API ([639ccee](https://github.com/ewhauser/bazel-mcp/commit/639cceeffc9e7e4ea70e4d879b348794b9e29e0b))
* **release:** publish assets through REST API ([8c3e3bc](https://github.com/ewhauser/bazel-mcp/commit/8c3e3bca6ea85ea88bee828c0b9a516cd3a040e7))
* **release:** upload assets to existing releases ([4aa029b](https://github.com/ewhauser/bazel-mcp/commit/4aa029bb0e7be0fe1b69a772e450bd3311d312c4))
* **release:** upload assets to existing releases ([6ac7357](https://github.com/ewhauser/bazel-mcp/commit/6ac7357ef13a73f1153719395a2c009233d1e460))
* **runner:** surface Rust test failure evidence ([53533b0](https://github.com/ewhauser/bazel-mcp/commit/53533b0ee961c543ac071694a9b1e10120e288df))
* **runner:** surface Rust test failure evidence ([22fcbc2](https://github.com/ewhauser/bazel-mcp/commit/22fcbc2893f861e6ef52040768f4c9336030788a))


### Performance Improvements

* **runner:** add opt-in POSIX BEP FIFO ([a3661c7](https://github.com/ewhauser/bazel-mcp/commit/a3661c76aca38f482cc67256f326b5ce57dccfd1))
* **runner:** reduce BEP incrementally ([3a846d2](https://github.com/ewhauser/bazel-mcp/commit/3a846d2c59e6b774316ee7449d6b7139ead3c5b7))
* **runner:** reduce BEP incrementally with opt-in POSIX FIFO ([206a983](https://github.com/ewhauser/bazel-mcp/commit/206a9837bf152a997580f87f89bf4cdc5b1d85ba))

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
