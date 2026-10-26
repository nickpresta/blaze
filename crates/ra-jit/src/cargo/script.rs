//! Build scripts of the workspace. Cargo runs one again when what it said it reads has
//! changed, or when anything in its package has if it said nothing, and then compiles the
//! package again whatever the script printed. The daemon runs the script the same way and
//! looks at what it printed and wrote: when that is what it was, the crates compiled with
//! it are still right, and the build stays the daemon's.
//!
//! To run a script as cargo does, the wrapper puts a few lines of shell where rustc linked
//! it. Cargo runs those, and they run `ra-jit build-script`, which writes down the
//! environment and the directory cargo ran it in and runs the script. Every run, cargo's and
//! the daemon's, is [`script_sandbox::run`]'s, in the sandbox tests run in.

use std::{
    ffi::OsString,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime},
};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::{sandboxed, script_sandbox};

/// The variable that names the script itself to `ra-jit build-script`.
const REAL: &str = "RA_JIT_BUILD_SCRIPT";

/// The file next to a script's `OUT_DIR` that says how cargo ran it.
const RAN: &str = "ra-jit-ran.json";

/// The file next to a script's `OUT_DIR` whose time is when [`rerun_is_same`] last found it
/// the same.
const CHECKED: &str = "ra-jit-checked";

/// How cargo ran a build script.
#[derive(Serialize, Deserialize)]
struct Ran {
    real: PathBuf,
    cwd: PathBuf,
    env: Vec<(String, String)>,
}

/// Has cargo run the script rustc linked at `linked` through `ra-jit build-script`.
#[cfg(unix)]
pub(crate) fn stand_in_front(linked: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let real = real_of(linked);
    std::fs::rename(linked, &real)?;
    let exe = std::env::current_exe()?;
    let quoted = |path: &Path| -> anyhow::Result<String> {
        let path = path.to_str().context("a path that is not text")?;
        Ok(shlex::try_quote(path)?.into_owned())
    };
    let lines = format!(
        "#!/bin/sh\n{REAL}={} exec {} build-script \"$@\"\n",
        quoted(&real)?,
        quoted(&exe)?
    );
    std::fs::write(linked, lines)?;
    Ok(std::fs::set_permissions(linked, std::fs::Permissions::from_mode(0o755))?)
}

/// Has cargo run the script rustc linked at `linked` through `ra-jit build-script`. There is
/// no shell to say so in a file cargo runs, so this executable is linked there under the
/// script's name, and [`stood_in_front_of`] is how it knows. Where it can't be linked, on
/// another volume, cargo runs the script itself and a change it reruns for is cargo's to
/// build.
#[cfg(windows)]
pub(crate) fn stand_in_front(linked: &Path) -> anyhow::Result<()> {
    let real = real_of(linked);
    std::fs::rename(linked, &real)?;
    if std::fs::hard_link(std::env::current_exe()?, linked).is_err() {
        std::fs::rename(&real, linked)?;
    }
    Ok(())
}

/// The script this executable stands in front of, when it runs under the name of one:
/// the name rustc linked it under, or the one cargo gives it in the same directory.
#[cfg(windows)]
pub(crate) fn stood_in_front_of() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let name = exe.file_stem()?.to_str()?;
    if !name.starts_with("build_script_") && !name.starts_with("build-script-") {
        return None;
    }
    let beside = std::fs::read_dir(exe.parent()?).ok()?.flatten().map(|it| it.path());
    beside.into_iter().find(|it| it.to_str().is_some_and(|it| it.ends_with(".exe.real")))
}

fn real_of(linked: &Path) -> PathBuf {
    let mut real = linked.as_os_str().to_owned();
    real.push(".real");
    PathBuf::from(real)
}

/// `ra-jit build-script`: writes down how cargo runs the script and becomes it.
pub(crate) fn command(args: impl Iterator<Item = OsString>) -> anyhow::Result<()> {
    let real = PathBuf::from(std::env::var_os(REAL).context("no build script is named")?);
    run_as_cargo_does(real, args)
}

/// Writes down how cargo runs the script at `real`, and runs it as [`script_sandbox::plan`]
/// says, or has a run the store kept stand for it.
pub(crate) fn run_as_cargo_does(
    real: PathBuf,
    args: impl Iterator<Item = OsString>,
) -> anyhow::Result<()> {
    let env: Vec<(String, String)> = std::env::vars_os()
        .filter(|(name, _)| name != REAL)
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect();
    let out_dir =
        env.iter().find(|(name, _)| name == "OUT_DIR").map(|(_, value)| PathBuf::from(value));
    let args: Vec<OsString> = args.collect();
    let cwd = std::env::current_dir()?;
    let mut script = Command::new(&real);
    script.args(&args).env_remove(REAL);
    // Not run by cargo: there is nowhere for it to write.
    let Some(out_dir) = out_dir else { return become_it(&mut script) };
    let run_dir = out_dir.parent().context("an OUT_DIR at the root")?.to_owned();
    let places = Places::of(&env);
    let key = |places: Option<&Places>| run_key(&real, &cwd, &args, &env, places);
    let (shared, here) = (key(Some(&places)), key(None));
    let ran = Ran { real: real.clone(), cwd: cwd.clone(), env };
    // Without it the script is not run again by the daemon, and cargo builds.
    let _ = std::fs::write(run_dir.join(RAN), serde_json::to_vec(&ran)?);
    let running = script_sandbox::Script {
        real: &real,
        args: &args,
        cwd: &cwd,
        env: &ran.env,
        out_dir: &out_dir,
        scratch: &run_dir,
    };
    let plan = script_sandbox::plan(&running)?;
    ra_jit_store::compiles_against(RUNS);
    let store =
        here.and_then(|_| ra_jit_store::Store::global_or(super::daemon::store().as_deref()));
    if store.is_none()
        && let script_sandbox::Plan::Outside(_) = plan
    {
        script_sandbox::keep_reads(&out_dir, None);
        return become_it(&mut script);
    }
    let keys = shared.zip(here).zip(store);
    // A run another checkout or target directory kept, or, when what it left names where it
    // ran in a file that is no text, one of this place.
    let kept = keys.as_ref().and_then(|((shared, here), store)| {
        match store.get(shared).and_then(|it| Kept::read(&it)) {
            Some(kept) => Some(kept.placed(&places)),
            None => store.get(here).and_then(|it| Kept::read(&it)),
        }
    });
    if let Some(kept) = kept.filter(|it| it.still(&cwd)) {
        kept.restore(&out_dir)?;
        let reads: Vec<String> = kept.reads.iter().map(|it| it.0.clone()).collect();
        let reads = (!script_sandbox::says_what_it_reads(&kept.stdout)).then_some(&reads[..]);
        script_sandbox::keep_reads(&out_dir, reads);
        // Cargo keeps what a script printed to stderr beside its `OUT_DIR`.
        if std::env::var_os("RA_JIT_STATS").is_some() {
            eprintln!("ra-jit: a run the store kept");
        }
        std::io::stdout().write_all(&kept.stdout)?;
        std::io::stderr().write_all(&kept.stderr)?;
        return Ok(());
    }
    let script_sandbox::Ran { output, reads } =
        script_sandbox::run(&running, &plan, Stdio::inherit())?;
    std::io::stdout().write_all(&output.stdout)?;
    std::io::stderr().write_all(&output.stderr)?;
    let reads = reads.filter(|_| !script_sandbox::says_what_it_reads(&output.stdout));
    script_sandbox::keep_reads(&out_dir, reads.as_deref());
    if output.status.success()
        && let Some(((shared, here), store)) = keys
        && let Some(kept) = Kept::of(output.stdout, output.stderr, &out_dir, &cwd, reads.as_deref())
    {
        store.settled();
        match kept.unplaced(&places) {
            Some(unplaced) => store.insert(&shared, &unplaced.bytes()),
            None => store.insert(&here, &kept.bytes()),
        }
        store.flush();
    }
    std::process::exit(output.status.code().unwrap_or(1))
}

/// What the store's runs of build scripts are kept under in place of a toolchain: what a run
/// prints and writes is told by the script, which its key has.
const RUNS: &str = "build script runs 2";

/// The variables of a run that change nothing it prints or writes: the jobs it may start, and
/// the directories the shell that ran cargo was in, which cargo leaves as they were (it runs
/// the script in its package's directory). Whether it runs in a sandbox changes nothing it
/// prints or writes either: a run that the sandbox refused something fails, and a failed run
/// is not kept.
const UNREAD: [&str; 8] = [
    "CARGO_MAKEFLAGS",
    "MAKEFLAGS",
    "MFLAGS",
    "NUM_JOBS",
    "PWD",
    "OLDPWD",
    sandboxed::VAR,
    "BLAZE_SANDBOX",
];

/// The most a run may leave for the store to keep it.
const MOST_KEPT: usize = 256 << 20;

/// The key of a run of the script `real` in `cwd` with `args` and `env`. With `places`, what
/// names the checkout and the target directory names them by what stands for them, so a run
/// in another checkout or target directory has the same key. `None` when the script cannot
/// be read.
fn run_key(
    real: &Path,
    cwd: &Path,
    args: &[OsString],
    env: &[(String, String)],
    places: Option<&Places>,
) -> Option<[u8; 32]> {
    let mut hash = Sha256::new();
    let mut field = |bytes: &[u8]| {
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    };
    field(RUNS.as_bytes());
    let text = |bytes: &[u8]| match places {
        Some(places) => places.unplace(bytes),
        None => bytes.to_vec(),
    };
    if places.is_some() {
        field(b"places apart");
    }
    field(&Sha256::digest(std::fs::read(real).ok()?));
    field(&text(cwd.as_os_str().as_encoded_bytes()));
    for arg in args {
        field(&text(arg.as_encoded_bytes()));
    }
    let mut env: Vec<&(String, String)> =
        env.iter().filter(|(name, _)| !UNREAD.contains(&name.as_str())).collect();
    env.sort();
    for (name, value) in env {
        field(name.as_bytes());
        field(&text(value.as_bytes()));
    }
    Some(hash.finalize().into())
}

/// The places of one checkout a run of a build script names, each with what stands for it in
/// what the store keeps: the target directory its `OUT_DIR` is below, up to cargo's `build`
/// directory, and its package's directory, where cargo runs it. No path holds a NUL.
#[derive(Debug)]
struct Places(Vec<(Vec<u8>, &'static [u8])>);

impl Places {
    const TARGET: &[u8] = b"\0TARGET\0";
    const PACKAGE: &[u8] = b"\0PACKAGE\0";

    fn of(env: &[(String, String)]) -> Places {
        let var = |name: &str| {
            let value = env.iter().find(|it| it.0 == name)?.1.trim_end_matches(['/', '\\']);
            (!value.is_empty()).then_some(value)
        };
        let mut places = Vec::new();
        if let Some(out) = var("OUT_DIR")
            && let Some(at) = out.replace('\\', "/").rfind("/build/")
            && at > 0
        {
            places.push((out.as_bytes()[..at].to_vec(), Self::TARGET));
        }
        if let Some(package) = var("CARGO_MANIFEST_DIR") {
            places.push((package.as_bytes().to_vec(), Self::PACKAGE));
        }
        // The longer first: the target directory is often in the package's.
        places.sort_by_key(|it| std::cmp::Reverse(it.0.len()));
        Places(places)
    }

    /// `bytes` with what stands for each place in place of it.
    fn unplace(&self, bytes: &[u8]) -> Vec<u8> {
        let mut bytes = bytes.to_vec();
        for (place, mark) in &self.0 {
            bytes = replaced(&bytes, place, mark);
        }
        bytes
    }

    /// `bytes` with each place where [`Places::unplace`] put what stands for it.
    fn place(&self, bytes: &[u8]) -> Vec<u8> {
        let mut bytes = bytes.to_vec();
        for (place, mark) in &self.0 {
            bytes = replaced(&bytes, mark, place);
        }
        bytes
    }

    /// Whether `bytes` name a place.
    fn named_in(&self, bytes: &[u8]) -> bool {
        self.0.iter().any(|(place, _)| holds(bytes, place))
    }

    /// Whether `bytes` hold what stands for a place, and so could not be told from bytes that
    /// named it.
    fn marked(&self, bytes: &[u8]) -> bool {
        self.0.iter().any(|(_, mark)| holds(bytes, mark))
    }
}

fn holds(bytes: &[u8], part: &[u8]) -> bool {
    !part.is_empty() && bytes.windows(part.len()).any(|it| it == part)
}

/// `bytes` with every `from` in them `to`.
fn replaced(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while !from.is_empty()
        && let Some(at) = rest.windows(from.len()).position(|it| it == from)
    {
        out.extend_from_slice(&rest[..at]);
        out.extend_from_slice(to);
        rest = &rest[at + from.len()..];
    }
    out.extend_from_slice(rest);
    out
}

/// What a run of a build script printed and left in its `OUT_DIR`, and what the files it said
/// it reads had in them: the run stands for another with the same key while they still do.
#[derive(Debug, PartialEq)]
struct Kept {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Each file below `OUT_DIR`, by its path there, whether it is executable, and its bytes.
    files: Vec<(String, bool, Vec<u8>)>,
    /// Each file or directory of `rerun-if-changed`, as the script named it, and the digest
    /// of what it had: nothing when it was not there.
    watched: Vec<(String, Vec<u8>)>,
    /// For a script that said nothing of what it reads, each path the sandbox saw it read, and
    /// what [`sandboxed::digest`] said of it: the whole of its inputs, its record being whole.
    reads: Vec<(String, String)>,
}

impl Kept {
    /// The run that printed `stdout` and wrote `out_dir`, run in `cwd`, which read `reads` when
    /// that is known ([`script_sandbox::Ran::reads`]). `None` when cargo would run it again for
    /// a change to any file of its package, which neither the key nor what it read says, or
    /// when it left too much.
    fn of(
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        out_dir: &Path,
        cwd: &Path,
        reads: Option<&[String]>,
    ) -> Option<Kept> {
        let printed = String::from_utf8_lossy(&stdout);
        let mut watched = Vec::new();
        let mut says_what_it_reads = false;
        for line in printed.lines() {
            let Some(line) = line.strip_prefix("cargo:") else { continue };
            let line = line.strip_prefix(':').unwrap_or(line);
            if let Some(path) = line.strip_prefix("rerun-if-changed=") {
                watched.push((path.to_owned(), watched_digest(&cwd.join(path))));
                says_what_it_reads = true;
            }
            says_what_it_reads |= line.starts_with("rerun-if-env-changed=");
        }
        // A script that said nothing is run again by cargo for any file of its package; what
        // the sandbox saw it read, all of it, is what a run depends on.
        let reads = match (says_what_it_reads, reads) {
            (true, _) => Vec::new(),
            (false, Some(reads)) => {
                reads.iter().map(|it| (it.clone(), sandboxed::digest(Path::new(it)))).collect()
            }
            (false, None) => return None,
        };
        let mut files = Vec::new();
        let mut size = stdout.len() + stderr.len();
        let mut pending = vec![out_dir.to_owned()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).ok()?.flatten() {
                let path = entry.path();
                let kind = entry.file_type().ok()?;
                if kind.is_dir() {
                    pending.push(path);
                    continue;
                }
                let name = path.strip_prefix(out_dir).ok()?.to_str()?.to_owned();
                let bytes = std::fs::read(&path).ok()?;
                size += bytes.len();
                if size > MOST_KEPT {
                    return None;
                }
                files.push((name, is_executable(&entry.metadata().ok()?), bytes));
            }
        }
        files.sort();
        Some(Kept { stdout, stderr, files, watched, reads })
    }

    /// It with what stands for each place in place of it, for a run in any checkout or target
    /// directory to stand for. `None` when a file that is no text names a place, whose bytes
    /// a place of another length would break, or when something it holds could not be told
    /// from what stands for one.
    fn unplaced(&self, places: &Places) -> Option<Kept> {
        let texts = [&self.stdout, &self.stderr].into_iter();
        let files = self.files.iter().map(|it| &it.2);
        let paths = self.watched.iter().map(|it| it.0.as_bytes());
        let paths = paths.chain(self.reads.iter().map(|it| it.0.as_bytes()));
        let names = self.files.iter().map(|it| it.0.as_bytes());
        if texts
            .chain(files)
            .map(Vec::as_slice)
            .chain(paths)
            .chain(names)
            .any(|it| places.marked(it))
        {
            return None;
        }
        let mut files = Vec::with_capacity(self.files.len());
        for (name, executable, bytes) in &self.files {
            let bytes = match places.named_in(bytes) {
                true if std::str::from_utf8(bytes).is_err() => return None,
                true => places.unplace(bytes),
                false => bytes.clone(),
            };
            files.push((name.clone(), *executable, bytes));
        }
        let text = |it: &str| String::from_utf8(places.unplace(it.as_bytes())).ok();
        let watched = self.watched.iter().map(|(path, digest)| Some((text(path)?, digest.clone())));
        let reads = self.reads.iter().map(|(path, digest)| Some((text(path)?, digest.clone())));
        Some(Kept {
            stdout: places.unplace(&self.stdout),
            stderr: places.unplace(&self.stderr),
            files,
            watched: watched.collect::<Option<_>>()?,
            reads: reads.collect::<Option<_>>()?,
        })
    }

    /// What [`Kept::unplaced`] made of a run, with the places of this one.
    fn placed(self, places: &Places) -> Kept {
        let text = |it: &str| String::from_utf8_lossy(&places.place(it.as_bytes())).into_owned();
        let files = self.files.into_iter().map(|(name, executable, bytes)| {
            let bytes = if places.marked(&bytes) { places.place(&bytes) } else { bytes };
            (name, executable, bytes)
        });
        Kept {
            stdout: places.place(&self.stdout),
            stderr: places.place(&self.stderr),
            files: files.collect(),
            watched: self
                .watched
                .iter()
                .map(|(path, digest)| (text(path), digest.clone()))
                .collect(),
            reads: self.reads.iter().map(|(path, digest)| (text(path), digest.clone())).collect(),
        }
    }

    /// Whether every file it said it reads, and every path the sandbox saw it read, has what
    /// it had.
    fn still(&self, cwd: &Path) -> bool {
        self.watched.iter().all(|(path, digest)| watched_digest(&cwd.join(path)) == *digest)
            && self.reads.iter().all(|(path, digest)| sandboxed::digest(Path::new(path)) == *digest)
    }

    /// Writes the files it left into `out_dir`, as a run would.
    fn restore(&self, out_dir: &Path) -> anyhow::Result<()> {
        for (name, executable, bytes) in &self.files {
            let path = out_dir.join(name);
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
            if *executable {
                make_executable(&path)?;
            }
        }
        Ok(())
    }

    fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut put = |bytes: &[u8]| {
            out.extend((bytes.len() as u64).to_le_bytes());
            out.extend(bytes);
        };
        put(RUNS.as_bytes());
        put(&self.stdout);
        put(&self.stderr);
        put(&(self.files.len() as u64).to_le_bytes());
        for (name, executable, bytes) in &self.files {
            put(name.as_bytes());
            put(&[u8::from(*executable)]);
            put(bytes);
        }
        put(&(self.watched.len() as u64).to_le_bytes());
        for (path, digest) in &self.watched {
            put(path.as_bytes());
            put(digest);
        }
        put(&(self.reads.len() as u64).to_le_bytes());
        for (path, digest) in &self.reads {
            put(path.as_bytes());
            put(digest.as_bytes());
        }
        out
    }

    fn read(bytes: &[u8]) -> Option<Kept> {
        let rest = &mut &*bytes;
        let count = |it: &[u8]| Some(u64::from_le_bytes(it.try_into().ok()?) as usize);
        let text = |it: &[u8]| String::from_utf8(it.to_vec()).ok();
        if take(rest)? != RUNS.as_bytes() {
            return None;
        }
        let (stdout, stderr) = (take(rest)?.to_vec(), take(rest)?.to_vec());
        let mut files = Vec::new();
        for _ in 0..count(take(rest)?)? {
            let name = text(take(rest)?)?;
            let executable = take(rest)? == [1];
            files.push((name, executable, take(rest)?.to_vec()));
        }
        let mut watched = Vec::new();
        for _ in 0..count(take(rest)?)? {
            watched.push((text(take(rest)?)?, take(rest)?.to_vec()));
        }
        let mut reads = Vec::new();
        for _ in 0..count(take(rest)?)? {
            reads.push((text(take(rest)?)?, text(take(rest)?)?));
        }
        Some(Kept { stdout, stderr, files, watched, reads })
    }
}

/// The next field of `rest`, which is its length and then its bytes.
fn take<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
    let len = usize::try_from(u64::from_le_bytes(rest.get(..8)?.try_into().ok()?)).ok()?;
    let field = rest.get(8..8usize.checked_add(len)?)?;
    *rest = &rest[8 + len..];
    Some(field)
}

/// What the file or directory at `path` has: its bytes', or its names' and files' digest.
/// Nothing when it is not there.
fn watched_digest(path: &Path) -> Vec<u8> {
    match std::fs::metadata(path) {
        Ok(it) if it.is_dir() => digest_of(path),
        Ok(_) => std::fs::read(path).map(|it| Sha256::digest(it).to_vec()).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_: &std::fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
pub(crate) fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
pub(crate) fn make_executable(_: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn become_it(script: &mut Command) -> anyhow::Result<()> {
    Err(std::os::unix::process::CommandExt::exec(script).into())
}

#[cfg(not(unix))]
fn become_it(script: &mut Command) -> anyhow::Result<()> {
    std::process::exit(script.status()?.code().unwrap_or(1))
}

/// The file cargo kept what the script that wrote `out_dir` printed in, beside it: `output`
/// in cargo's first layout of its build directory, `run/stdout` in the second
/// (`build/<package>/<hash>/out`, the default of nightly cargo since 2026).
pub(crate) fn printed(out_dir: &Path) -> Option<PathBuf> {
    let run_dir = out_dir.parent()?;
    let second = run_dir.join("run").join("stdout");
    Some(if second.exists() { second } else { run_dir.join("output") })
}

/// Whether `path` can be in a script's `OUT_DIR`: below `build/<unit>/out` in cargo's first
/// layout or `build/<package>/<hash>/out` in its second, where the objects of a library are
/// too.
pub(crate) fn in_out_dir(path: &Path) -> bool {
    let names: Vec<&str> = path.components().filter_map(|c| c.as_os_str().to_str()).collect();
    names.windows(3).any(|it| it[0] == "build" && it[2] == "out")
        || names.windows(4).any(|it| it[0] == "build" && it[3] == "out")
}

/// Whether a script can be run again as cargo ran it into `out_dir`.
pub(crate) fn can_rerun(out_dir: &Path) -> bool {
    out_dir.parent().is_some_and(|it| it.join(RAN).exists())
}

/// The directory cargo ran the script that wrote `out_dir` in, which is its package's, when
/// it was written down.
pub(crate) fn ran_in(out_dir: &Path) -> Option<PathBuf> {
    let ran = std::fs::read(out_dir.parent()?.join(RAN)).ok()?;
    serde_json::from_slice::<Ran>(&ran).ok().map(|it| it.cwd)
}

/// Whether the script that wrote `out_dir` is as cargo would compile it now: no file rustc
/// read for it (`.d` beside it, named from `workspace`, where cargo runs rustc) is newer than
/// it, and it read no variable while compiled but cargo's own, as cargo set them to run it:
/// cargo compiles it again when one changes.
pub(crate) fn compiled_from_what_is_there(out_dir: &Path, workspace: &Path) -> bool {
    let Some(run_dir) = out_dir.parent() else { return false };
    let Ok(ran) = std::fs::read(run_dir.join(RAN)) else { return false };
    let Ok(ran) = serde_json::from_slice::<Ran>(&ran) else { return false };
    let Some(linked) = ran.real.to_str().and_then(|it| it.strip_suffix(".real")) else {
        return false;
    };
    let compiled = std::fs::metadata(&ran.real).and_then(|it| it.modified());
    let Ok(read) = std::fs::read_to_string(Path::new(linked).with_extension("d")) else {
        return false;
    };
    let Ok(compiled) = compiled else { return false };
    let read = crate::dep_info::DepInfo::parse(&read);
    // Cargo sets its own from the manifest, which a change of has the checkout loaded again;
    // whoever runs cargo sets the others.
    let set = |name: &str| ran.env.iter().find(|it| it.0 == name).map(|it| it.1.as_str());
    if read
        .env
        .iter()
        .any(|(name, value)| !name.starts_with("CARGO_") || set(name) != value.as_deref())
    {
        return false;
    }
    !read.files.is_empty()
        && read.files.iter().all(|it| {
            let written = std::fs::metadata(workspace.join(it)).and_then(|it| it.modified());
            written.is_ok_and(|it| it <= compiled)
        })
}

/// Runs the script that wrote `out_dir` again, as cargo ran it but into a directory of its
/// own: what cargo's holds is read by the compilers of a build under way, and cargo
/// compiles the package again when a file there is newer than it was. `Ok` when the script
/// printed and wrote what it did then; `Err` is what is different, which a script that
/// writes the name of its directory into a file always is.
pub(crate) fn rerun_is_same(out_dir: &Path) -> Result<(), String> {
    // A file written as the script reads it can have a time a little before now: the
    // kernel stamps files with a clock that lags by up to a tick.
    let began = SystemTime::now() - 2 * file_clock_tick();
    rerun_matches(out_dir, out_dir)?;
    if let Some(run_dir) = out_dir.parent()
        && let Ok(checked) = std::fs::File::create(run_dir.join(CHECKED))
    {
        let _ = checked.set_modified(began);
    }
    Ok(())
}

/// How far behind the clock the kernel stamps a file it writes can be: Linux takes the time
/// of the last tick.
#[cfg(target_os = "linux")]
fn file_clock_tick() -> Duration {
    let mut tick = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `tick` is a timespec to write to.
    match unsafe { libc::clock_getres(libc::CLOCK_REALTIME_COARSE, &mut tick) } {
        0 => Duration::new(tick.tv_sec as u64, tick.tv_nsec as u32).max(Duration::from_millis(1)),
        _ => Duration::from_millis(20),
    }
}

/// Windows stamps files with the system time, which moves in steps of about 16 ms.
#[cfg(windows)]
fn file_clock_tick() -> Duration {
    Duration::from_millis(16)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn file_clock_tick() -> Duration {
    Duration::from_millis(1)
}

/// Whether the script that wrote `out_dir` was run again by [`rerun_is_same`], and printed
/// and wrote what it had, after `newest`: when nothing it reads is newer, a run now is the
/// same again.
pub(crate) fn checked_after(out_dir: &Path, newest: SystemTime) -> bool {
    let checked = out_dir.parent().map(|it| it.join(CHECKED));
    let checked = checked.and_then(|it| std::fs::metadata(it).and_then(|it| it.modified()).ok());
    checked.is_some_and(|it| newest < it)
}

/// Whether the run of a script that wrote `out_dir` printed and wrote what the run that wrote
/// `other` did, but for the name of its directory.
pub(crate) fn same_run(out_dir: &Path, other: &Path) -> bool {
    let printed = |dir: &Path| std::fs::read(printed(dir)?).ok();
    let (Some(ours), Some(theirs)) = (printed(out_dir), printed(other)) else { return false };
    let (Some(named), Some(names)) = (other.to_str(), out_dir.to_str()) else { return false };
    String::from_utf8_lossy(&ours).replace(names, named).as_bytes() == theirs
        && digest_of(out_dir) == digest_of(other)
}

/// Runs the script that wrote `out_dir` again as [`rerun_is_same`] does, and compares what
/// it prints and writes with what a run of the same script left in `other`: another build of
/// the package, which ran it with the same source and inputs.
///
/// A script that said nothing of what it reads, whose run cargo's sandbox saw read only what
/// is as it was ([`script_sandbox::reads_unchanged`]), is not run: a run now would read the
/// same and so print and write the same.
pub(crate) fn rerun_matches(out_dir: &Path, other: &Path) -> Result<(), String> {
    if script_sandbox::reads_unchanged(out_dir) && (out_dir == other || same_run(out_dir, other)) {
        return Ok(());
    }
    let run_dir = out_dir.parent().ok_or("a build script without a directory")?;
    let other_dir = other.parent().ok_or("a build script without a directory")?;
    let read = |dir: &Path, name: &str| {
        std::fs::read(dir.join(name)).map_err(|it| format!("{name}: {it}"))
    };
    let ran: Ran = serde_json::from_slice(&read(run_dir, RAN)?).map_err(|it| it.to_string())?;
    let printed = printed(other).ok_or("a build script without a directory")?;
    let printed = std::fs::read(&printed).map_err(|it| {
        let name = printed.strip_prefix(other_dir).unwrap_or(&printed);
        format!("{}: {it}", name.display())
    })?;
    let again_dir = run_dir.join(format!("ra-jit-out-{}", std::process::id()));
    let scratch = run_dir.join(format!("ra-jit-again-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&again_dir);
    std::fs::create_dir(&again_dir).map_err(|it| it.to_string())?;
    let same = rerun_into(&ran, &again_dir, &scratch, other, &printed);
    let _ = std::fs::remove_dir_all(&again_dir);
    let _ = std::fs::remove_dir_all(&scratch);
    same
}

/// Runs the script as cargo ran it, but into `again_dir`, in the sandbox cargo's run was in
/// ([`script_sandbox::run`]), with its spec and record in `scratch`.
fn rerun_into(
    ran: &Ran,
    again_dir: &Path,
    scratch: &Path,
    out_dir: &Path,
    printed: &[u8],
) -> Result<(), String> {
    let again_text = again_dir.to_str().ok_or("has a directory whose name is not text")?;
    let env: Vec<(String, String)> = ran
        .env
        .iter()
        .map(|(name, value)| match name == "OUT_DIR" {
            true => (name.clone(), again_text.to_owned()),
            false => (name.clone(), value.clone()),
        })
        .collect();
    let script = script_sandbox::Script {
        real: &ran.real,
        args: &[],
        cwd: &ran.cwd,
        env: &env,
        out_dir: again_dir,
        scratch,
    };
    let plan = script_sandbox::plan(&script).map_err(|it| format!("{it:#}"))?;
    let again =
        script_sandbox::run(&script, &plan, Stdio::null()).map_err(|it| format!("{it:#}"))?;
    let again = again.output;
    if !again.status.success() {
        return Err(format!("failed ({})", again.status));
    }
    let (Some(named), Some(names)) = (out_dir.to_str(), again_dir.to_str()) else {
        return Err("has a directory whose name is not text".to_owned());
    };
    if String::from_utf8_lossy(&again.stdout).replace(names, named).as_bytes() != printed {
        return Err("printed something else".to_owned());
    }
    if digest_of(again_dir) != digest_of(out_dir) {
        return Err("wrote something else".to_owned());
    }
    Ok(())
}

/// A hash of the names, as they are below `dir`, and the contents of the files there.
fn digest_of(dir: &Path) -> Vec<u8> {
    let mut hash = Sha256::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(below) = pending.pop() {
        let entries = std::fs::read_dir(&below).into_iter().flatten().flatten();
        let mut paths: Vec<PathBuf> = entries.map(|it| it.path()).collect();
        paths.sort();
        for path in paths {
            let name = path.strip_prefix(dir).unwrap_or(&path);
            hash.update(name.as_os_str().as_encoded_bytes());
            match path.is_dir() {
                true => pending.push(path),
                false => hash.update(std::fs::read(&path).unwrap_or_default()),
            }
        }
    }
    hash.finalize().to_vec()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A run directory as cargo and `ra-jit build-script` leave one, for a script that
    /// writes `wrote` and prints where.
    fn ran(name: &str, wrote: &str) -> PathBuf {
        let run_dir =
            std::env::temp_dir().join(format!("ra-jit-script-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&run_dir);
        let out_dir = run_dir.join("out");
        std::fs::create_dir_all(&out_dir).unwrap();
        let real = run_dir.join("script");
        let lines =
            "#!/bin/sh\nprintf %s \"$WROTE\" > \"$OUT_DIR/made.rs\"\necho \"cargo:in=$OUT_DIR\"\n";
        // A process a test thread starts while this one has the script open to write holds it
        // open too until that process execs, and the script is refused meanwhile ("text file
        // busy"): another process writes it.
        let written = Command::new("sh")
            .args(["-c", "printf %s \"$1\" > \"$0\" && chmod 755 \"$0\""])
            .arg(&real)
            .arg(lines)
            .status()
            .unwrap();
        assert!(written.success());
        let env = vec![
            ("OUT_DIR".to_owned(), out_dir.to_str().unwrap().to_owned()),
            ("WROTE".to_owned(), wrote.to_owned()),
            ("PATH".to_owned(), std::env::var("PATH").unwrap()),
            // The test's executable is no `ra-jit sandbox`.
            (sandboxed::VAR.to_owned(), "off".to_owned()),
        ];
        let ran = Ran { real, cwd: run_dir.clone(), env };
        std::fs::write(run_dir.join(RAN), serde_json::to_vec(&ran).unwrap()).unwrap();
        std::fs::write(run_dir.join("output"), format!("cargo:in={}\n", out_dir.display()))
            .unwrap();
        out_dir
    }

    #[test]
    fn a_rerun_leaves_what_cargo_has_alone() {
        let out_dir = ran("same", "fn made() {}");
        let made = out_dir.join("made.rs");
        std::fs::write(&made, "fn made() {}").unwrap();
        let written = std::fs::metadata(&made).unwrap().modified().unwrap();
        assert_eq!(rerun_is_same(&out_dir), Ok(()));
        assert_eq!(std::fs::metadata(&made).unwrap().modified().unwrap(), written);
        let left: Vec<_> =
            std::fs::read_dir(out_dir.parent().unwrap()).unwrap().flatten().collect();
        assert_eq!(left.len(), 5, "{left:?}");
        let before = SystemTime::now() - Duration::from_secs(1);
        assert!(checked_after(&out_dir, before));
        assert!(!checked_after(&out_dir, SystemTime::now()), "a file written since");
    }

    #[test]
    fn a_run_is_the_same_as_another_but_for_its_directory() {
        let out_dir = ran("run", "fn made() {}");
        let other = ran("run-other", "fn made() {}");
        std::fs::write(out_dir.join("made.rs"), "fn made() {}").unwrap();
        std::fs::write(other.join("made.rs"), "fn made() {}").unwrap();
        assert!(same_run(&out_dir, &other));
        std::fs::write(other.join("made.rs"), "fn made_before() {}").unwrap();
        assert!(!same_run(&out_dir, &other));
    }

    #[test]
    fn a_rerun_is_compared_with_what_the_other_run_left() {
        let out_dir = ran("matches", "fn made() {}");
        let other = ran("matches-other", "fn made() {}");
        std::fs::write(other.join("made.rs"), "fn made() {}").unwrap();
        assert_eq!(rerun_matches(&out_dir, &other), Ok(()));
        std::fs::write(other.join("made.rs"), "fn made_for_the_load() {}").unwrap();
        assert_eq!(rerun_matches(&out_dir, &other), Err("wrote something else".to_owned()));
    }

    #[test]
    fn a_kept_run_stands_for_another_while_what_it_reads_is_the_same() {
        let out_dir = ran("kept", "fn made() {}");
        let cwd = out_dir.parent().unwrap().to_owned();
        std::fs::create_dir_all(out_dir.join("deep")).unwrap();
        std::fs::write(out_dir.join("deep/made.rs"), "fn made() {}").unwrap();
        std::fs::write(cwd.join("read.txt"), "one").unwrap();
        let stdout = b"cargo::rerun-if-changed=read.txt\ncargo:rustc-cfg=kept\n".to_vec();
        let kept = Kept::of(stdout, b"a warning".to_vec(), &out_dir, &cwd, None).unwrap();
        let bytes = kept.bytes();
        assert_eq!(Kept::read(&bytes), Some(kept));
        let kept = Kept::read(&bytes).unwrap();
        assert!(kept.still(&cwd));
        let again = cwd.join("again");
        kept.restore(&again).unwrap();
        assert_eq!(std::fs::read_to_string(again.join("deep/made.rs")).unwrap(), "fn made() {}");
        std::fs::write(cwd.join("read.txt"), "two").unwrap();
        assert!(!kept.still(&cwd));
        let undeclared = || b"cargo:rustc-cfg=x\n".to_vec();
        assert!(Kept::of(undeclared(), Vec::new(), &out_dir, &cwd, None).is_none());
        // What the sandbox saw it read stands for what it would have said.
        let read = cwd.join("read.txt").to_str().unwrap().to_owned();
        let kept = Kept::of(undeclared(), Vec::new(), &out_dir, &cwd, Some(&[read])).unwrap();
        assert_eq!(Kept::read(&kept.bytes()).as_ref(), Some(&kept));
        assert!(kept.still(&cwd));
        std::fs::write(cwd.join("read.txt"), "three").unwrap();
        assert!(!kept.still(&cwd));
    }

    #[test]
    fn a_run_in_another_checkout_has_its_key_and_what_it_left_names_that_checkout() {
        let root =
            std::env::temp_dir().join(format!("ra-jit-script-places-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let real = root.join("script");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&real, "a script").unwrap();
        let checkout = |name: &str| {
            let at = root.join(name);
            let (package, out) = (at.join("pkg"), at.join("target/debug/build/pkg-0123/out"));
            std::fs::create_dir_all(&out).unwrap();
            std::fs::create_dir_all(&package).unwrap();
            std::fs::write(package.join("read.txt"), "one").unwrap();
            let env = vec![
                ("CARGO_MANIFEST_DIR".to_owned(), package.to_str().unwrap().to_owned()),
                ("OUT_DIR".to_owned(), out.to_str().unwrap().to_owned()),
                ("PATH".to_owned(), "/bin".to_owned()),
                ("PWD".to_owned(), at.to_str().unwrap().to_owned()),
            ];
            (package, out, env)
        };
        let (package, out, env) = checkout("one");
        let (other_package, other_out, other_env) = checkout("other");
        let (places, other_places) = (Places::of(&env), Places::of(&other_env));
        let key = |cwd: &Path, env: &[(String, String)], places: Option<&Places>| {
            run_key(&real, cwd, &[], env, places)
        };
        assert_eq!(
            key(&package, &env, Some(&places)),
            key(&other_package, &other_env, Some(&other_places))
        );
        assert_ne!(key(&package, &env, None), key(&other_package, &other_env, None));
        let named = |out: &Path, package: &Path| Kept {
            stdout: format!(
                "cargo:rustc-link-search=native={}\ncargo:rerun-if-changed={}\n",
                out.display(),
                package.join("read.txt").display()
            )
            .into_bytes(),
            stderr: Vec::new(),
            files: vec![
                (
                    "made.rs".to_owned(),
                    false,
                    format!("include!(\"{}/x.rs\");", out.display()).into_bytes(),
                ),
                ("lib.a".to_owned(), false, vec![0xff, 0, 1]),
            ],
            watched: vec![(
                package.join("read.txt").to_str().unwrap().to_owned(),
                watched_digest(&package.join("read.txt")),
            )],
            reads: Vec::new(),
        };
        let unplaced = named(&out, &package).unplaced(&places).unwrap();
        let read = Kept::read(&unplaced.bytes()).unwrap();
        let placed = read.placed(&other_places);
        assert_eq!(placed, named(&other_out, &other_package));
        assert!(placed.still(&other_package));
        let mut binary = named(&out, &package);
        binary.files[1].2 = [&[0xff][..], out.to_str().unwrap().as_bytes()].concat();
        assert_eq!(binary.unplaced(&places), None, "a file that is no text naming the place");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Has the script of `out_dir` be one rustc compiled from `sources`, as `.d` says.
    fn compiled(out_dir: &Path, sources: &str) {
        let run_dir = out_dir.parent().unwrap();
        let mut ran: Ran =
            serde_json::from_slice(&std::fs::read(run_dir.join(RAN)).unwrap()).unwrap();
        let linked = run_dir.join("build_script_build-0123");
        if ran.real != linked.with_extension("real") {
            std::fs::copy(&ran.real, linked.with_extension("real")).unwrap();
        }
        ran.real = linked.with_extension("real");
        // A copy on macOS keeps the time of the file it was copied from.
        let real = std::fs::File::options().write(true).open(&ran.real).unwrap();
        real.set_modified(SystemTime::now()).unwrap();
        std::fs::write(run_dir.join(RAN), serde_json::to_vec(&ran).unwrap()).unwrap();
        std::fs::write(linked.with_extension("d"), format!("{}: {sources}\n", linked.display()))
            .unwrap();
    }

    #[test]
    fn a_script_newer_than_its_source_is_as_cargo_would_compile_it() {
        let out_dir = ran("fresh", "fn made() {}");
        let source = out_dir.parent().unwrap().join("build source.rs");
        std::fs::write(&source, "fn main() {}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        compiled(&out_dir, "build\\ source.rs");
        let workspace = out_dir.parent().unwrap();
        assert!(compiled_from_what_is_there(&out_dir, workspace));
        assert!(
            !compiled_from_what_is_there(&out_dir, Path::new("/elsewhere")),
            "a source that is not there"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&source, "fn main() { }").unwrap();
        assert!(!compiled_from_what_is_there(&out_dir, workspace));
    }

    #[test]
    fn a_script_that_read_a_variable_or_has_no_dep_info_is_cargos() {
        let out_dir = ran("env", "fn made() {}");
        let workspace = out_dir.parent().unwrap();
        assert!(!compiled_from_what_is_there(&out_dir, workspace), "no .d");
        let source = out_dir.parent().unwrap().join("build.rs");
        std::fs::write(&source, "fn main() {}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        compiled(&out_dir, "build.rs\n\n# env-dep:SOME_VARIABLE=1");
        assert!(!compiled_from_what_is_there(&out_dir, workspace));
        // One of cargo's own is as cargo set it to run the script, or not.
        let at = workspace.join(RAN);
        let mut ran: Ran = serde_json::from_slice(&std::fs::read(&at).unwrap()).unwrap();
        ran.env.push(("CARGO_PKG_VERSION".to_owned(), "1.0.0".to_owned()));
        std::fs::write(&at, serde_json::to_vec(&ran).unwrap()).unwrap();
        compiled(&out_dir, "build.rs\n\n# env-dep:CARGO_PKG_VERSION=1.0.0");
        assert!(compiled_from_what_is_there(&out_dir, workspace));
        compiled(&out_dir, "build.rs\n\n# env-dep:CARGO_PKG_VERSION=2.0.0");
        assert!(!compiled_from_what_is_there(&out_dir, workspace));
    }

    /// cargo's second layout of its build directory keeps what a script printed in
    /// `run/stdout`, not in `output`.
    #[test]
    fn a_run_in_the_second_layout_is_read_from_run_stdout() {
        let out_dir = ran("second", "fn made() {}");
        let run_dir = out_dir.parent().unwrap();
        std::fs::create_dir(run_dir.join("run")).unwrap();
        std::fs::rename(run_dir.join("output"), run_dir.join("run/stdout")).unwrap();
        std::fs::write(out_dir.join("made.rs"), "fn made() {}").unwrap();
        assert_eq!(printed(&out_dir), Some(run_dir.join("run/stdout")));
        assert_eq!(rerun_is_same(&out_dir), Ok(()));
        let other = ran("second-other", "fn made() {}");
        std::fs::write(other.join("made.rs"), "fn made() {}").unwrap();
        assert_eq!(rerun_matches(&out_dir, &other), Ok(()));
        assert!(same_run(&out_dir, &other));
        assert!(in_out_dir(Path::new("/t/debug/build/app/0123/out/made.rs")));
        assert!(in_out_dir(Path::new("/t/debug/build/app-0123/out/made.rs")));
        assert!(!in_out_dir(Path::new("/t/debug/deps/app-0123.d")));
    }

    #[test]
    fn a_rerun_that_writes_something_else_is_told() {
        let out_dir = ran("other", "fn made() {}");
        std::fs::write(out_dir.join("made.rs"), "fn made_before() {}").unwrap();
        assert_eq!(rerun_is_same(&out_dir), Err("wrote something else".to_owned()));
        assert_eq!(std::fs::read(out_dir.join("made.rs")).unwrap(), b"fn made_before() {}");
    }
}
