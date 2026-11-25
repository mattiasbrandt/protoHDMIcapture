# Changelog

## [0.6.0](https://github.com/mattiasbrandt/protoHDMIcapture/compare/v0.5.1...v0.6.0) (2026-11-25)


### Features

* **dashboard:** GPU use while an enhancement is on; new-only's and openfg's numbers read right ([#72](https://github.com/mattiasbrandt/protoHDMIcapture/issues/72)) ([b16083c](https://github.com/mattiasbrandt/protoHDMIcapture/commit/b16083cc538ed11415cdb0cbbd21a850123e09ad))
* **dashboard:** kernel→presented against the baseline, VRAM at 90 and 97 percent ([#76](https://github.com/mattiasbrandt/protoHDMIcapture/issues/76)) ([11a2dd6](https://github.com/mattiasbrandt/protoHDMIcapture/commit/11a2dd69f6a2c9bc6905c0516e59471de1701c10))
* **dashboard:** the source's frame rate and cadence, from its duplicate frames ([#65](https://github.com/mattiasbrandt/protoHDMIcapture/issues/65)) ([f31d543](https://github.com/mattiasbrandt/protoHDMIcapture/commit/f31d5431058cdf4648a76b35114c65cd1a235e6c))
* **framegen:** FRAMEGEN=off|ffx|rife, the frame generation a viewer starts with ([#84](https://github.com/mattiasbrandt/protoHDMIcapture/issues/84)) ([209780b](https://github.com/mattiasbrandt/protoHDMIcapture/commit/209780b115b5821149d28d93588b515217c76142))
* **gpu:** ask GameMode for performance while a GPU enhancement is on ([#77](https://github.com/mattiasbrandt/protoHDMIcapture/issues/77)) ([af3f0a8](https://github.com/mattiasbrandt/protoHDMIcapture/commit/af3f0a8a59f6004680860fdfe0b6da13751aa09d))
* **gpu:** results name the GPU, driver and display server ([#87](https://github.com/mattiasbrandt/protoHDMIcapture/issues/87)) ([4b19f50](https://github.com/mattiasbrandt/protoHDMIcapture/commit/4b19f50518785f06005d4596802823be1a6c5ca7))
* **gpu:** the GPU section reads NVIDIA through NVML ([#87](https://github.com/mattiasbrandt/protoHDMIcapture/issues/87)) ([890be21](https://github.com/mattiasbrandt/protoHDMIcapture/commit/890be21ce0dad94cd5fcf822d0b846b0d96eef5d))
* **keys:** g cycles frame generation in every build, on the overlay's Frame generation row ([#84](https://github.com/mattiasbrandt/protoHDMIcapture/issues/84)) ([9c41d14](https://github.com/mattiasbrandt/protoHDMIcapture/commit/9c41d1454b2db2988c74b0f82839129b9ddbe94e))
* **scaling:** c steps the picture's size; c and g work with the overlay closed ([#85](https://github.com/mattiasbrandt/protoHDMIcapture/issues/85)) ([e92e8d9](https://github.com/mattiasbrandt/protoHDMIcapture/commit/e92e8d99dccaaabb3977e4337947cf9d86fac0ac))
* **scaling:** chroma upsampled with spline36, not linear ([#85](https://github.com/mattiasbrandt/protoHDMIcapture/issues/85)) ([faf984b](https://github.com/mattiasbrandt/protoHDMIcapture/commit/faf984b268035404bda6f52044002fd716fe42ed))
* **scaling:** Upscaling, its own setting: AMD FSR 1 or NVIDIA Image Scaling ([#85](https://github.com/mattiasbrandt/protoHDMIcapture/issues/85)) ([7bd51d2](https://github.com/mattiasbrandt/protoHDMIcapture/commit/7bd51d2d65a6bd0cfef727373b2b4409e81b0496))
* **stats:** a run that switches frame generation is summed per mode, and repeats by time ([#86](https://github.com/mattiasbrandt/protoHDMIcapture/issues/86)) ([69c597f](https://github.com/mattiasbrandt/protoHDMIcapture/commit/69c597ff45fa9b55dac0cf2d4b4a76db124e65bb))
* **stats:** the GPU summary says when a pass ran slow, and a p99 without the start ([#82](https://github.com/mattiasbrandt/protoHDMIcapture/issues/82)) ([fa056de](https://github.com/mattiasbrandt/protoHDMIcapture/commit/fa056de49b32b8f0c4a2462789996524a24189b1))
* **wizard:** a GPU enhancements step turns Upscaling on by itself ([#85](https://github.com/mattiasbrandt/protoHDMIcapture/issues/85)) ([93586f2](https://github.com/mattiasbrandt/protoHDMIcapture/commit/93586f2e28d80739e7493cad13adaf0afac83612))
* **wizard:** the GPU enhancements step offers frame generation, off ([#84](https://github.com/mattiasbrandt/protoHDMIcapture/issues/84)) ([f90cd51](https://github.com/mattiasbrandt/protoHDMIcapture/commit/f90cd5135b6227591453bdf1b78ae9d113c1e3cf))


### Bug Fixes

* **audio:** a real-time refusal names RLIMIT_RTTIME, and a test guards the threads' class ([#90](https://github.com/mattiasbrandt/protoHDMIcapture/issues/90)) ([d0a8e15](https://github.com/mattiasbrandt/protoHDMIcapture/commit/d0a8e15fede7c86b5ac75307f1b988f8d5c044b4))
* **audio:** the audio threads get real-time again under PipeWire with RTKit ([#76](https://github.com/mattiasbrandt/protoHDMIcapture/issues/76)) ([8a6a65f](https://github.com/mattiasbrandt/protoHDMIcapture/commit/8a6a65fd5e8a4e4995b2a06c240b8f94869cd278))
* **clip:** a full disk leaves a clip that reads to its last whole frame ([#79](https://github.com/mattiasbrandt/protoHDMIcapture/issues/79)) ([5fc2071](https://github.com/mattiasbrandt/protoHDMIcapture/commit/5fc2071e118ede3718c1db9184d95d3df9195149))
* **dashboard:** duplicate-frame thresholds tuned on a real card ([#65](https://github.com/mattiasbrandt/protoHDMIcapture/issues/65)) ([7643483](https://github.com/mattiasbrandt/protoHDMIcapture/commit/7643483c1db6d4a320fea8495c9a04cc911550f1))
* **dashboard:** the not-answering line names protoHDMIcapture, not the viewer ([#76](https://github.com/mattiasbrandt/protoHDMIcapture/issues/76)) ([f420995](https://github.com/mattiasbrandt/protoHDMIcapture/commit/f420995dab081fcaa5138698215f5dad226ddc53))
* **dashboard:** values with a known threshold coloured by it ([#76](https://github.com/mattiasbrandt/protoHDMIcapture/issues/76)) ([9db2dc3](https://github.com/mattiasbrandt/protoHDMIcapture/commit/9db2dc39294bc51c78c8836c1d6a72d4e3aaab19))
* **dashboard:** warnings orange and errors red, whatever the terminal theme ([#76](https://github.com/mattiasbrandt/protoHDMIcapture/issues/76)) ([fd7bc17](https://github.com/mattiasbrandt/protoHDMIcapture/commit/fd7bc17d70c3787628db59623ccb37443c9ac4dc))
* **framegen:** a late refresh costs one generated frame, and every new frame is counted ([#78](https://github.com/mattiasbrandt/protoHDMIcapture/issues/78)) ([86d056d](https://github.com/mattiasbrandt/protoHDMIcapture/commit/86d056d1c770b312ce760fc1e42ab914c5224c36))
* **framegen:** rife is refused a picture too large for the GPU's buffers ([#74](https://github.com/mattiasbrandt/protoHDMIcapture/issues/74)) ([870bca7](https://github.com/mattiasbrandt/protoHDMIcapture/commit/870bca725a7cad9ebe71d735f9309aa5f3dc8f4b))
* **gamemode:** a registration or unregister with no answer is undone too ([#77](https://github.com/mattiasbrandt/protoHDMIcapture/issues/77)) ([8830aaf](https://github.com/mattiasbrandt/protoHDMIcapture/commit/8830aaf8c0f2f76446a681ac4e14d7b32cf64513))
* **legacy:** no warning for settings only protoHDMIcapture honours ([#85](https://github.com/mattiasbrandt/protoHDMIcapture/issues/85)) ([13f7e4c](https://github.com/mattiasbrandt/protoHDMIcapture/commit/13f7e4c5ed4546e32e5460e189f2f4ccb04f6a01))
* **overlay:** the Upscaling hint names c only where a larger size fits ([#85](https://github.com/mattiasbrandt/protoHDMIcapture/issues/85)) ([7775b49](https://github.com/mattiasbrandt/protoHDMIcapture/commit/7775b495a3c2d75170c504519dea922f746fa5de))
* **packaging:** the release ships the MIT notices of the code and data it ports ([#84](https://github.com/mattiasbrandt/protoHDMIcapture/issues/84)) ([75d3a2d](https://github.com/mattiasbrandt/protoHDMIcapture/commit/75d3a2d84d0179c70af16e1160e55b81c9f3f282))
* **run:** the capture stops at the run's end, not once the window has gone ([#79](https://github.com/mattiasbrandt/protoHDMIcapture/issues/79)) ([1324fbf](https://github.com/mattiasbrandt/protoHDMIcapture/commit/1324fbf1e8edd81b59a53abb5ecc4d8290a89db6))
* **stats:** a GPU sample counts in the mode its frame was submitted in ([#86](https://github.com/mattiasbrandt/protoHDMIcapture/issues/86)) ([77ecdb7](https://github.com/mattiasbrandt/protoHDMIcapture/commit/77ecdb77c28cf8494417138bc5dda7b6cc205787))
* **stats:** a run that switched modes prints no mixed frame-generation p99 ([#86](https://github.com/mattiasbrandt/protoHDMIcapture/issues/86)) ([b83ea4d](https://github.com/mattiasbrandt/protoHDMIcapture/commit/b83ea4de7f8ecaa9a54b678643ed3179ad217e05))
* **wizard:** the GPU enhancements step follows a move to another monitor ([#85](https://github.com/mattiasbrandt/protoHDMIcapture/issues/85)) ([38cd3c1](https://github.com/mattiasbrandt/protoHDMIcapture/commit/38cd3c1cff63c6a15a188fc33d38ae8f1fbdc054))
* **wizard:** Upscaling is offered as the monitor's resolution, not sharpening ([#84](https://github.com/mattiasbrandt/protoHDMIcapture/issues/84)) ([8a64c5a](https://github.com/mattiasbrandt/protoHDMIcapture/commit/8a64c5a228e72c553addf1865f0eb3dc09763452))

## [0.5.1](https://github.com/mattiasbrandt/protoHDMIcapture/compare/v0.5.0...v0.5.1) (2026-11-17)


### Bug Fixes

* **wizard:** the environment warning names only what wins over the file ([739e9ed](https://github.com/mattiasbrandt/protoHDMIcapture/commit/739e9ed63bdd7c118eac789f217fb33b262a8f56))

## [0.5.0](https://github.com/mattiasbrandt/protoHDMIcapture/compare/v0.4.0...v0.5.0) (2026-11-17)


### Features

* **wizard:** the source's setup tips show over its live picture ([871dfbe](https://github.com/mattiasbrandt/protoHDMIcapture/commit/871dfbe56316562265d253d1f3ae61bcc98a44f1))


### Bug Fixes

* **wizard:** the audio sync stage says when its sound stops, and r restarts it ([eb1f14e](https://github.com/mattiasbrandt/protoHDMIcapture/commit/eb1f14ee93a570936111a488e48b1933a4fa32dd))

## [0.4.0](https://github.com/mattiasbrandt/protoHDMIcapture/compare/v0.3.0...v0.4.0) (2026-11-17)


### Features

* **viewer:** --version and -V print the version ([e270839](https://github.com/mattiasbrandt/protoHDMIcapture/commit/e2708399b027bd0ca283f0be22c5eae5c95c99e5))

## [0.3.0](https://github.com/mattiasbrandt/protoHDMIcapture/compare/v0.2.0...v0.3.0) (2026-11-17)


### Features

* **audio:** say when real-time priority is refused ([51a170c](https://github.com/mattiasbrandt/protoHDMIcapture/commit/51a170cf8aec1f5fa707152411f3c67a40918aca))

## [0.2.0](https://github.com/mattiasbrandt/protoHDMIcapture/compare/v0.1.2...v0.2.0) (2026-11-17)


### Features

* **viewer:** --stats reports the overlay's CPU time per frame drawn ([3328adf](https://github.com/mattiasbrandt/protoHDMIcapture/commit/3328adfd373c4d3668fefc033e84429bbd83e69d))
* **viewer:** u in the overlay opens the setup wizard ([9498289](https://github.com/mattiasbrandt/protoHDMIcapture/commit/949828985a21c20e438986a23880babf2b03be28))

## [0.1.2](https://github.com/mattiasbrandt/protoHDMIcapture/compare/v0.1.1...v0.1.2) (2026-11-17)


### Bug Fixes

* **audio:** a loopback death no wake reached is said as the feed ends ([#42](https://github.com/mattiasbrandt/protoHDMIcapture/issues/42)) ([2533dbb](https://github.com/mattiasbrandt/protoHDMIcapture/commit/2533dbb9db1ffe76159ad494682b71bf1cd3ffbd))
* **card:** no card, or one not on USB, is said before PipeWire is asked ([#43](https://github.com/mattiasbrandt/protoHDMIcapture/issues/43)) ([4ee6e03](https://github.com/mattiasbrandt/protoHDMIcapture/commit/4ee6e03d3b799eb9c24eee7d36b2d74321024931))
* **viewer:** user-facing messages name protoHDMIcapture and legacy/view.sh ([#47](https://github.com/mattiasbrandt/protoHDMIcapture/issues/47)) ([a52b64c](https://github.com/mattiasbrandt/protoHDMIcapture/commit/a52b64c46780c0b25392f8d925feff321a636700))
* **wizard:** refresh matching offered on Linux only ([#43](https://github.com/mattiasbrandt/protoHDMIcapture/issues/43)) ([db9ff65](https://github.com/mattiasbrandt/protoHDMIcapture/commit/db9ff65094dc7a302191e2011a84879bc1c2e5bb))
