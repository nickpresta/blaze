//! Every run of a build script, cargo's (through the stand-in `script.rs` puts where rustc
//! linked it) and the daemon's own reruns, goes through [`run`], which runs it in the sandbox
//! every test runs in ([`crate::sandbox`]): the machine read-only, `OUT_DIR` written for real,
//! empty temporary directories, no network but the sandbox's own loopback, and exactly the
//! environment cargo gave it. "Everything should by default be in sandbox" (Andrew,
//! 2026-10-26; [#147]).
//!
//! Why `OUT_DIR` alone is written: it is the one place cargo tells a script to write ("Build
//! scripts may save any output files or intermediate artifacts in the directory specified in
//! the OUT_DIR environment variable. Scripts should not modify any files outside of that
//! directory", the Cargo book's build-scripts chapter). Cargo itself enforces nothing, so a
//! script that writes into its source or the target directory works under cargo; here that
//! write is refused, and the failure says what was refused and how to take the package out
//! (the alternative, an overlay that keeps the write private, would make it vanish silently).
//! On macOS the per-user cache directory (`getconf DARWIN_USER_CACHE_DIR`) is written for real
//! too: clang's module cache, the Metal compiler's and xcrun's live there, found by `confstr`
//! and not by a variable a private one could be named in, and they check what they cache.
//!
//! The package's directory, the workspace and the target directory are seen as they are
//! ([`Spec::readable`]) even under `/tmp`, which a sandbox otherwise has empty.
//!
//! Where it can't run sandboxed it fails, saying why and how to fix it, as `cargo ra-jit
//! test` does: the ways out are [`sandboxed::WORKSPACE_TABLE`] in the workspace's root
//! manifest (`unsandboxed-build-scripts`, the only place a registry's package can be named),
//! `build-script = false` under [`sandboxed::TABLE`] of a package inside the workspace, and
//! `RA_JIT_SANDBOX=off`.
//!
//! What a sandboxed run read is recorded ([`Spec::record_reads`]), and kept beside `OUT_DIR`
//! ([`READS`]) and with the run in the store (`script.rs`'s `Kept`). The rule that uses it,
//! conservative on purpose: cargo runs a script again when a file it named in
//! `rerun-if-changed` changed, when a variable of `rerun-if-env-changed` did, and, when it
//! named neither, when any file of its package did. Only in that last case does the record
//! decide anything: a run whose record is whole (no `X` line, which a program macOS starts
//! without the recorder gives) and whose every read path is as it was stands for a run now,
//! and is not run again. A script that named what it reads is run again exactly when cargo
//! says, and one whose record is not whole is run whenever cargo would run it.
//!
//! [#147]: https://github.com/indexable-inc/blaze/issues/147

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use anyhow::Context as _;

use super::sandboxed;
use crate::sandbox::{self, Line, Reached, Spec};

/// The file beside a script's `OUT_DIR` with what its last run of cargo's read, when that is
/// what decides whether it runs again ([`reads_unchanged`]): a first line [`READS_HEAD`], then
/// what [`sandboxed::stat`] and [`sandboxed::digest`] said of each path, and the path, by tabs.
pub(crate) const READS: &str = "ra-jit-reads";

const READS_HEAD: &str = "ra-jit build script reads 1";

/// One run of a build script.
pub(crate) struct Script<'a> {
    /// The script rustc or ra-jit linked.
    pub(crate) real: &'a Path,
    pub(crate) args: &'a [OsString],
    /// Its package's directory, where cargo runs it.
    pub(crate) cwd: &'a Path,
    /// All of its environment, as cargo gave it, with `OUT_DIR` where it writes.
    pub(crate) env: &'a [(String, String)],
    pub(crate) out_dir: &'a Path,
    /// A directory of the caller's for the sandbox's spec and record: written here, outside.
    pub(crate) scratch: &'a Path,
}

impl Script<'_> {
    fn var(&self, name: &str) -> Option<&str> {
        self.env.iter().find(|it| it.0 == name).map(|it| it.1.as_str())
    }

    fn package(&self) -> &str {
        self.var("CARGO_PKG_NAME").unwrap_or("?")
    }
}

/// How a run went.
pub(crate) struct Ran {
    pub(crate) output: Output,
    /// Each path it read that is an input, when it ran sandboxed and the record is whole.
    pub(crate) reads: Option<Vec<String>>,
}

/// Where a script runs.
pub(crate) enum Plan {
    /// Outside, and why.
    Outside(String),
    Sandboxed {
        spec: Spec,
        root_manifest: Option<PathBuf>,
    },
}

/// Whether sandboxes are on for a script run with `env`, as [`sandbox::on_with`] says. The
/// variable is read from the run's environment, which is cargo's, so that the daemon's reruns
/// do as cargo's runs did.
fn on_in(env: &[(String, String)]) -> bool {
    sandbox::on_with(|name| env.iter().find(|it| it.0 == name).map(|it| it.1.clone()))
}

/// The workspace a script's run is part of: the one the shell that ran cargo was in, else the
/// one around the target directory its `OUT_DIR` is in.
fn workspace_of(script: &Script<'_>) -> anyhow::Result<Option<PathBuf>> {
    if let Some(pwd) = script.var("PWD")
        && let Some(root) = sandboxed::workspace_root(Path::new(pwd))?
    {
        return Ok(Some(root));
    }
    match target_of(script.out_dir).and_then(Path::parent) {
        Some(dir) => sandboxed::workspace_root(dir),
        None => Ok(None),
    }
}

/// The target directory `out_dir` is in: what is above its `build` directory's profile.
fn target_of(out_dir: &Path) -> Option<&Path> {
    let build = out_dir.ancestors().find(|it| it.file_name().is_some_and(|it| it == "build"))?;
    build.parent()?.parent()
}

/// Where `script` runs: outside when sandboxes are off or its package is taken out, else the
/// sandbox's spec. `Err` is a manifest that says it wrongly.
pub(crate) fn plan(script: &Script<'_>) -> anyhow::Result<Plan> {
    let package = script.package();
    if !on_in(script.env) {
        return Ok(Plan::Outside(format!("{}=off", sandboxed::VAR)));
    }
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        return Ok(Plan::Outside(format!("there is no sandbox on {} yet", std::env::consts::OS)));
    }
    let root = workspace_of(script)?;
    if let Some(root) = &root {
        let out = sandboxed::workspace_opt_out(root)?;
        if out.build_scripts.contains(package) {
            let why = format!("`{package}` is in `unsandboxed-build-scripts`");
            return Ok(Plan::Outside(format!("{why} in {}", out.manifest.display())));
        }
        // A package of the workspace may take its script out in its own manifest; one from a
        // registry or git, which is below cargo's home, can't take itself out.
        let home = script.var("CARGO_HOME").map(PathBuf::from);
        if script.cwd.starts_with(root)
            && !home.is_some_and(|it| script.cwd.starts_with(it))
            && sandboxed::opt_out(script.cwd)?.build_script_outside
        {
            return Ok(Plan::Outside(format!(
                "`build-script = false` in {}",
                script.cwd.join("Cargo.toml").display()
            )));
        }
    }
    let mut writable = vec![script.out_dir.to_owned()];
    writable.extend(user_cache());
    let mut readable = vec![script.cwd.to_owned()];
    readable.extend(root.clone());
    readable.extend(target_of(script.out_dir).map(Path::to_owned));
    // The programs cargo names, and the directories of `PATH`, where a sandbox has empty
    // directories: cargo's `RUSTC` is ra-jit's own, which may be built below `/dev/shm`.
    let programs = ["RUSTC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "RUSTDOC", "CARGO"];
    let programs = programs.into_iter().filter_map(|it| script.var(it)).filter(|it| !it.is_empty());
    let programs = programs.filter_map(|it| Path::new(it).parent().map(Path::to_owned));
    let path = script.var("PATH").unwrap_or_default().split(':').map(PathBuf::from);
    // One reached through a link (NixOS's `/run/current-system/sw/bin`) is seen where the link
    // leads, which is not emptied.
    readable.extend(programs.chain(path).filter(|it| {
        it.is_dir() && emptied(it) && std::fs::canonicalize(it).is_ok_and(|real| emptied(&real))
    }));
    readable.sort();
    readable.dedup();
    // The innermost is enough: a directory is seen with what is below it.
    let all = readable.clone();
    readable.retain(|it| !all.iter().any(|other| other != it && it.starts_with(other)));
    let mut not_inputs = vec![script.real.to_owned()];
    not_inputs.extend(std::fs::canonicalize(script.real).ok());
    let spec = Spec {
        cwd: script.cwd.to_owned(),
        env: script.env.to_vec(),
        overlays: Vec::new(),
        writable,
        readable,
        record: Some(script.scratch.join("ra-jit-record")),
        record_reads: true,
        not_inputs,
    };
    Ok(Plan::Sandboxed { spec, root_manifest: root.map(|it| it.join("Cargo.toml")) })
}

/// Whether `dir` is below a directory a sandbox starts empty: what is there is not seen unless
/// it is readable.
fn emptied(dir: &Path) -> bool {
    let temps = ["/tmp", "/var/tmp", "/dev/shm", "/run", "/private/tmp", "/private/var/tmp"];
    let temp = std::env::temp_dir();
    let real = std::fs::canonicalize(&temp).unwrap_or_else(|_| temp.clone());
    dir.is_absolute()
        && (temps.iter().any(|it| dir.starts_with(it))
            || dir.starts_with(&temp)
            || dir.starts_with(&real))
}

/// The per-user cache directory of macOS, `None` elsewhere.
#[cfg(target_os = "macos")]
fn user_cache() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;
    let mut buffer = [0u8; 1024];
    // SAFETY: `buffer` is writable for its length.
    let len = unsafe {
        libc::confstr(libc::_CS_DARWIN_USER_CACHE_DIR, buffer.as_mut_ptr().cast(), buffer.len())
    };
    if len == 0 || len > buffer.len() {
        return None;
    }
    let dir = PathBuf::from(std::ffi::OsStr::from_bytes(&buffer[..len - 1]));
    Some(std::fs::canonicalize(&dir).unwrap_or(dir))
}

#[cfg(not(target_os = "macos"))]
fn user_cache() -> Option<PathBuf> {
    None
}

/// Runs `script` as `plan` says, with `stdin`, its outputs kept. A failed run in the sandbox
/// has a note after what it printed to stderr, naming the package, what the sandbox refused
/// it and how to take it out; a sandbox that could not be made, why and how to fix it.
pub(crate) fn run(script: &Script<'_>, plan: &Plan, stdin: Stdio) -> anyhow::Result<Ran> {
    let (spec, root_manifest) = match plan {
        Plan::Outside(why) => {
            if std::env::var_os("RA_JIT_STATS").is_some() {
                eprintln!("ra-jit: the build script of `{}` runs outside: {why}", script.package());
            }
            let mut command = Command::new(script.real);
            command.args(script.args).env_clear().envs(script.env.iter().map(|(k, v)| (k, v)));
            command.current_dir(script.cwd).stdin(stdin);
            let output = command.output().with_context(|| running(script))?;
            return Ok(Ran { output, reads: None });
        }
        Plan::Sandboxed { spec, root_manifest } => (spec, root_manifest),
    };
    std::fs::create_dir_all(script.scratch)
        .with_context(|| format!("making {}", script.scratch.display()))?;
    let spec_file = script.scratch.join("ra-jit-sandbox.json");
    std::fs::write(&spec_file, serde_json::to_vec(spec)?)
        .with_context(|| format!("writing {}", spec_file.display()))?;
    let record = spec.record.clone().context("a sandbox with no record")?;
    _ = std::fs::remove_file(&record);
    let mut command = Command::new(std::env::current_exe().context("finding ra-jit itself")?);
    command.arg(sandbox::COMMAND).arg(&spec_file).arg("--").arg(script.real).args(script.args);
    command.current_dir(script.cwd).stdin(stdin);
    let mut output = command.output().with_context(|| running(script))?;
    let lines = sandbox::read_record(&record);
    let package = script.package();
    if output.status.code() == Some(sandbox::COULD_NOT)
        && lines.is_none()
        && let Err(why) = sandbox::available()
    {
        output.stderr.extend(
            format!(
                "error: the build script of `{package}` runs in a sandbox, which can't be made \
                 here: {why}\n`{}=off` runs build scripts and tests outside a sandbox, as cargo \
                 does\n",
                sandboxed::VAR
            )
            .into_bytes(),
        );
    } else if !output.status.success()
        && let Some(lines) = &lines
        && let Some(note) = refused_note(lines, package, root_manifest.as_deref())
    {
        output.stderr.extend(format!("note: {note}\n").into_bytes());
    }
    let reads = lines.filter(|it| it.iter().all(|it| it.reached != Reached::Incomplete));
    let reads = reads.map(|lines| inputs(&lines, script, spec));
    Ok(Ran { output, reads })
}

fn running(script: &Script<'_>) -> String {
    format!("running the build script of `{}` ({})", script.package(), script.real.display())
}

/// What the sandbox refused a script that failed, and how to take it out; `None` when it
/// refused nothing.
fn refused_note(lines: &[Line], package: &str, root_manifest: Option<&Path>) -> Option<String> {
    let mut them: Vec<&Line> =
        lines.iter().filter(|it| matches!(it.reached, Reached::Write | Reached::Net)).collect();
    if them.is_empty() {
        return None;
    }
    them.sort_by(|a, b| (a.reached, &a.what).cmp(&(b.reached, &b.what)));
    them.dedup_by(|a, b| a.reached == b.reached && a.what == b.what);
    let root = root_manifest
        .map_or("the workspace's root manifest".to_owned(), |it| it.display().to_string());
    Some(format!(
        "the build script of `{package}` failed in the sandbox, which refused it {}; if it \
         needs that, run it outside: `unsandboxed-build-scripts = [{package:?}]` under `{}` in \
         {root} (or `build-script = false` under `{}` in its own manifest, for a package of the \
         workspace), or `{}=off` for every build script and test of the command",
        sandboxed::what_was_refused(&them),
        sandboxed::WORKSPACE_TABLE,
        sandboxed::TABLE,
        sandboxed::VAR,
    ))
}

/// The paths of `lines` that are inputs of the run: not what it wrote (`OUT_DIR`, the
/// writable cache), not the temporary directories and the kernel's views, which are empty or
/// the run's own, unless a readable directory is there (a workspace below `/dev/shm`), and not
/// the script itself, which the run's key has.
fn inputs(lines: &[Line], script: &Script<'_>, spec: &Spec) -> Vec<String> {
    let mut written: Vec<PathBuf> = spec.writable.clone();
    written.push(script.scratch.to_owned());
    let mut own: Vec<PathBuf> =
        ["/dev", "/proc", "/sys", "/tmp", "/private/tmp", "/var/tmp"].map(PathBuf::from).into();
    own.extend(["/private/var/tmp", "/private/var/folders", "/var/folders"].map(PathBuf::from));
    own.push(std::env::temp_dir());
    let input = |path: &Path| {
        if written.iter().any(|it| path.starts_with(it))
            || spec.not_inputs.iter().any(|it| it == path)
        {
            return false;
        }
        spec.readable.iter().any(|it| path.starts_with(it))
            || !own.iter().any(|it| path.starts_with(it))
    };
    let mut paths: Vec<String> = lines
        .iter()
        .filter(|it| it.reached == Reached::Read && input(Path::new(&it.what)))
        .map(|it| it.what.clone())
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// Writes down beside `out_dir` what the run of cargo's that wrote it read, `reads`, for
/// [`reads_unchanged`]; or that nothing is known, with `None`, which is what a run that named
/// what it reads, or ran outside or with a record that is not whole, leaves.
pub(crate) fn keep_reads(out_dir: &Path, reads: Option<&[String]>) {
    let Some(run_dir) = out_dir.parent() else { return };
    let file = run_dir.join(READS);
    let Some(reads) = reads else {
        _ = std::fs::remove_file(&file);
        return;
    };
    let mut text = format!("{READS_HEAD}\n");
    for path in reads {
        let it = Path::new(path);
        text.push_str(&format!("{}\t{}\t{path}\n", sandboxed::stat(it), sandboxed::digest(it)));
    }
    let writing = file.with_extension("writing");
    if std::fs::write(&writing, text).and_then(|()| std::fs::rename(&writing, &file)).is_err() {
        _ = std::fs::remove_file(&file);
    }
}

/// Whether the run that wrote `out_dir` read, in the sandbox, nothing that is not as it was
/// then, by what [`keep_reads`] wrote down: a run now would print and write the same. False
/// when nothing was written down: what it read is not known.
pub(crate) fn reads_unchanged(out_dir: &Path) -> bool {
    let Some(file) = out_dir.parent().map(|it| it.join(READS)) else { return false };
    let Ok(text) = std::fs::read_to_string(file) else { return false };
    let mut lines = text.lines();
    if lines.next() != Some(READS_HEAD) {
        return false;
    }
    lines.all(|line| {
        let mut parts = line.splitn(3, '\t');
        let (Some(stat), Some(digest), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            return false;
        };
        let path = Path::new(path);
        sandboxed::stat(path) == stat || sandboxed::digest(path) == digest
    })
}

/// Whether a script that printed `stdout` said what it reads: a file or a variable cargo runs
/// it again for. One that said neither is run again for any file of its package.
pub(crate) fn says_what_it_reads(stdout: &[u8]) -> bool {
    String::from_utf8_lossy(stdout).lines().any(|line| {
        let Some(line) = line.strip_prefix("cargo:") else { return false };
        let line = line.strip_prefix(':').unwrap_or(line);
        line.starts_with("rerun-if-changed=") || line.starts_with("rerun-if-env-changed=")
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_run_whose_reads_are_as_they_were_needs_no_rerun() {
        let dir = std::env::temp_dir().join(format!("ra-jit-script-reads-{}", std::process::id()));
        let out_dir = dir.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let read = dir.join("read.txt");
        std::fs::write(&read, "one").unwrap();
        assert!(!reads_unchanged(&out_dir), "nothing written down");
        keep_reads(&out_dir, Some(&[read.to_str().unwrap().to_owned()]));
        assert!(reads_unchanged(&out_dir));
        std::fs::write(&read, "two").unwrap();
        assert!(!reads_unchanged(&out_dir));
        keep_reads(&out_dir, None);
        assert!(!reads_unchanged(&out_dir));
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_script_says_what_it_reads_by_either_spelling() {
        assert!(says_what_it_reads(b"cargo:rustc-cfg=x\ncargo::rerun-if-changed=build.rs\n"));
        assert!(says_what_it_reads(b"cargo:rerun-if-env-changed=CC\n"));
        assert!(!says_what_it_reads(b"cargo:rustc-link-lib=z\n"));
    }

    #[test]
    fn what_was_refused_names_the_package_and_the_way_out() {
        let lines = [
            Line { reached: Reached::Write, test: String::new(), what: "/src/gen.rs".into() },
            Line { reached: Reached::Net, test: String::new(), what: "1.2.3.4:443".into() },
            Line { reached: Reached::Read, test: String::new(), what: "/src/x".into() },
        ];
        let note = refused_note(&lines, "sys-crate", Some(Path::new("/ws/Cargo.toml"))).unwrap();
        assert!(
            note.starts_with("the build script of `sys-crate` failed in the sandbox"),
            "{note}"
        );
        assert!(note.contains("writing `/src/gen.rs`") && note.contains("`1.2.3.4:443`"), "{note}");
        assert!(note.contains("`unsandboxed-build-scripts = [\"sys-crate\"]`"), "{note}");
        assert!(note.contains("/ws/Cargo.toml"), "{note}");
        assert!(refused_note(&lines[2..], "x", None).is_none());
    }

    #[test]
    fn what_a_workspace_below_dev_shm_read_is_an_input() {
        let read =
            |what: &str| Line { reached: Reached::Read, test: String::new(), what: what.into() };
        let lines = [
            read("/dev/shm/ws/pkg/VERSION"),
            read("/dev/shm/ws/target/debug/build/pkg-1/out/made.rs"),
            read("/dev/null"),
            read("/tmp/x"),
            read("/usr/lib/libc.so"),
        ];
        let out_dir = Path::new("/dev/shm/ws/target/debug/build/pkg-1/out");
        let script = Script {
            real: Path::new("/dev/shm/ws/target/debug/build/pkg-0/build_script_build.real"),
            args: &[],
            cwd: Path::new("/dev/shm/ws/pkg"),
            env: &[],
            out_dir,
            scratch: Path::new("/dev/shm/ws/target/debug/build/pkg-1"),
        };
        let spec = Spec {
            writable: vec![out_dir.to_owned()],
            readable: vec!["/dev/shm/ws".into()],
            ..Spec::default()
        };
        assert_eq!(inputs(&lines, &script, &spec), ["/dev/shm/ws/pkg/VERSION", "/usr/lib/libc.so"]);
    }

    #[test]
    fn the_target_directory_is_above_the_profile_of_build() {
        let out = Path::new("/w/target/debug/build/x-0123/out");
        assert_eq!(target_of(out), Some(Path::new("/w/target")));
        let out =
            Path::new("/w/target/x86_64-unknown-linux-gnu/debug/build/x/0123456789abcdef/out");
        assert_eq!(target_of(out), Some(Path::new("/w/target/x86_64-unknown-linux-gnu")));
    }
}
