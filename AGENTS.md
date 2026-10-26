# ra-jit

How to build, test and change ra-jit, including the vendored rust-analyzer, is
[docs/contributing.md](docs/contributing.md); what the harnesses check is
[docs/harness.md](docs/harness.md).

- **ra-jit never runs rustc.** ra-jit compiles everything itself: no function, crate,
  standard library, build script, proc macro, check, test, doctest or link step is handed to
  rustc, rustdoc or a cargo that runs them, and there is no "fall back to cargo" path. Each place
  that still does is a bug with a `no-rustc` issue, to be removed, never extended or added to:
  [docs/no-rustc.md](docs/no-rustc.md).
- **Own the whole stack, and stay a superset of Rust.** ra-jit moves step by step toward
  owning every layer from source to running code (front end, std, codegen, linking, loading),
  because owning it is what makes it fast. It never gets there by narrowing what it accepts:
  every program the pinned nightly's rustc builds (`harness/toolchain`, the latest nightly) must
  build and behave the same under ra-jit, with the real std, C dependencies, build scripts and
  proc macros. What ra-jit adds, such as live edits and
  tiers, comes on top. The one allowed difference is sandboxing: build scripts and proc macros
  run sandboxed ([#147](https://github.com/indexable-inc/blaze/issues/147), done by default
  since 2026-10-26). One that needs
  more than the sandbox allows is the only exception, and is made to work rather than refused
  where possible: [docs/no-rustc.md](docs/no-rustc.md).
- **Every test, build script and proc macro runs in a sandbox, by default, on every
  platform.** "Everything should by default be in sandbox and yea it should have sandbox
  support macos" (Andrew, 2026-10-26). `cargo ra-jit test` runs each test executable in one
  sandbox (Linux namespaces and seccomp, macOS Seatbelt and the interposer) under `all` and
  `affected` alike; every build script, cargo's runs and the daemon's, runs in the same one
  through `cargo/script_sandbox.rs` (writing only its `OUT_DIR`), and every proc-macro server
  is started in it (`macro_sandbox.rs`). Where none can be made it stops, rather than run
  outside; the only ways out are a package's `[package.metadata.blaze.sandbox]` (`tests =
  false`, `unsandboxed-tests`, `build-script = false`), the root manifest's
  `[workspace.metadata.blaze.sandbox]` (`unsandboxed-build-scripts`,
  `unsandboxed-proc-macros`) and `RA_JIT_SANDBOX=off`, never a silent fallback. Nothing new
  that runs user code runs outside it by default:
  [docs/cargo.md](docs/cargo.md#build-scripts-and-proc-macros-run-in-a-sandbox).
- **Pick the best long-term design, not the least work.** Agents write most of ra-jit, so the
  amount of code a design takes is not what limits it: own the layer, rewrite it or port
  rustc's rule properly, rather than do the least work or what a small team could maintain.
  What stays expensive is proving it right (differential tests against rustc, fuzzing, real
  crates), so that is where the effort goes: [docs/contributing.md](docs/contributing.md#rules-for-code).
- **Language semantics go in the rust-analyzer subtree.** MIR building and passes, borrow
  checking, const eval, layout and code-identity hashes as salsa queries (what rustc keeps in
  `rustc_middle`, `rustc_mir_transform`, `rustc_borrowck`) go in `vendor/rust-analyzer`, as a
  new crate there rather than edits to upstream's files where possible; codegen backends,
  object writing, the splice, the daemon, cargo integration, the store, tiers, the live runtime
  and the harnesses stay in `crates/`: [docs/contributing.md](docs/contributing.md#where-new-code-goes).
- **No MCP.** MCP is cringe and outdated; agent-facing interfaces are a Unix socket (local) or
  HTTP (remote) with one documented, versioned schema in deterministic CBOR (JSON only as a
  rendering for people), and the CLI is a client of the same API:
  [docs/roadmap.md](docs/roadmap.md#8-the-agent-api).
- **Heavy work runs on a dev host, the Mac does quick checks.** Release builds, harness sweeps
  and benchmarks run on an AWS dev host claimed in the host ledger (`fleet-ops` skill), with a
  lease as long as the run, released after: [docs/contributing.md](docs/contributing.md#working-in-parallel).
- **One worktree per lane, never `git stash`.** A uniquely named worktree with its own
  `CARGO_TARGET_DIR` or a shared sccache; the stash is shared by every worktree; harnesses run
  with `RUST_BACKTRACE=0`.
- **Land small and often, through the merge queue only.** After the local gate, run
  `python3 .github/scripts/land.py "what the branch does"` in the lane's worktree: it opens the
  pull request, queues it and waits until the fast tier (about 10 minutes) has passed on it
  merged onto main and it is merged, or says why not. Its waiting survives a tool that kills
  it; `land.py --wait` picks it up again. Never push to main or merge into it by
  hand: [docs/contributing.md](docs/contributing.md#landing-a-change).
- **Delete what a lane made when it is done:** its worktree, target directory and dev-host
  directories, and its merged branch.
- **Credit people.** A commit or pull request built on someone's suggestion, diagnosis,
  workaround or patch from an issue, review or discussion gets a
  `Co-authored-by: Name <id+login@users.noreply.github.com>` trailer for each of them, and names
  where it came from: [docs/contributing.md](docs/contributing.md#credit).
- **The site's narration is a podcast script, not the article read aloud.** Each page in
  `site/content/` has its own spoken version, written for the ear: an `Intro`, one `Say` per
  top-level Section or Issue (a `Say` on a container replaces its whole subtree; a long section
  splits into `Say`s on its sub-blocks, the rest `Silent`) and an `Outro`. Talk to the listener,
  say why before what, describe code by what it shows and never read symbols out, keep each `Say`
  under about 90 seconds, and steer the voice with Fish Audio Drama 3 directions (`[warm]`,
  `[curious]`, `[pause]`, `[laughs]`, `<emphasis>...</emphasis>`). It must stay accurate to the
  article: a change to a page changes its script in the same commit. Pronunciations go in
  `site/audio/lexicon.toml`, which never touches text inside `[ ]`:
  [site/audio/README.md](site/audio/README.md).
