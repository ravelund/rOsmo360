# Contributing to rOsmo360

Thanks for helping. rOsmo360 is a free, open replacement for DJI Studio's playback and editing of Osmo 360 footage, and help is welcome: bug reports, testing on hardware we don't have (Windows PCs with a GPU, other Macs, other Linux setups), fixes and features.

## Reporting a problem

Open an [issue](https://github.com/ravelund/rOsmo360/issues) with:

- what you did, what you expected and what happened;
- your system (OS, GPU), the rOsmo360 version (`rosmo360 --version`) and how you installed it;
- for stitching, stabilisation or colour problems, a short clip that shows it helps more than anything: a few seconds of `.OSV` with its `.LRF`, shared however you like. We only use it to find the bug, and never publish it.

## Code

1. For anything bigger than a small fix, open an issue first so we can agree on the approach.
2. Read [`AGENTS.md`](AGENTS.md): it covers building, the code layout, and the rules that are easy to break. The most important one: **the GPU and CPU paths must match.** Every drawing change goes in both the shader and the CPU stitcher, and is checked by comparing the two.
3. Keep a pull request to one change. Say in it what you changed and how you checked it (which footage, what you measured or compared).
4. `cargo build`, `cargo test` and `cargo clippy` should pass with no new warnings.
5. If users will see the change, update the [user guide](docs/user-guide.md) and the README in the same pull request.
6. Don't commit footage, exports or anything showing people.

Using an AI coding agent is fine: `AGENTS.md` is written for it. You're still responsible for what the PR contains, and for having run it.

## Licence

rOsmo360 is dual-licensed under [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE). Unless you say otherwise, anything you contribute is licensed the same way.
