//! `ra-jit weaken FILE...`: copies of rustc's Mach-O objects and archives whose definitions
//! give way to ra-jit's.
//!
//! ra-jit's object is linked ahead of what rustc built of the same crates, and defines many of
//! the same names. An ELF linker is told to keep the first definition it meets. Apple's has no
//! such flag: two definitions of a name are an error unless one is weak, and of several weak
//! ones the first on the command line stays. So every definition of the copies is made weak,
//! and private to the program as well, which keeps the loader from looking for the name in
//! the libraries it loads. rustc's copy of a static that is private to its object (rustc
//! keeps a static to the codegen unit that uses it, and LLVM may rename it) is made private to
//! the program instead, so that ra-jit's code can share it (`symbols::StaticCopy`). Nothing
//! else of a file changes, and a file that is no Mach-O object is copied as it is.

use std::{
    ops::Range,
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use object::{
    LittleEndian as LE,
    macho::{LC_SYMTAB, MachHeader64, N_EXT, N_PEXT, N_SECT, N_STAB, N_TYPE, N_WEAK_DEF, Nlist64},
    pod,
    read::{
        archive::ArchiveFile,
        macho::{MachHeader as _, Nlist as _, Segment as _},
    },
};

/// The directory of the copies, in the one the files are in. Named anew when the copies
/// change, so that none of an older ra-jit is taken for one of this.
pub(crate) const DIR: &str = "ra-jit-weak-2";

/// The directories of copies older ra-jits wrote, which this one deletes.
const OLD_DIRS: [&str; 1] = ["ra-jit-weak"];

/// Writes the copy of each file and prints where it is, in the order given.
pub(crate) fn command(args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    for file in args {
        println!("{}", copy_of(Path::new(&file))?.display());
    }
    Ok(())
}

/// Writes the copy of `file`, unless the one there is of the file as it is, and says where
/// it is.
pub(crate) fn copy_of(file: &Path) -> anyhow::Result<PathBuf> {
    let (Some(dir), Some(name)) = (file.parent(), file.file_name()) else {
        anyhow::bail!("{} names no file", file.display());
    };
    for old in OLD_DIRS {
        _ = std::fs::remove_dir_all(dir.join(old));
    }
    let dir = dir.join(DIR);
    std::fs::create_dir_all(&dir)?;
    let copy = dir.join(name);
    if !up_to_date(&copy, file) {
        let mut data =
            std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
        weaken(&mut data);
        crate::replace(&copy, &data)?;
    }
    Ok(copy)
}

/// Whether the copy was written after the file it is of.
fn up_to_date(copy: &Path, file: &Path) -> bool {
    let written = |path: &Path| std::fs::metadata(path).and_then(|it| it.modified()).ok();
    matches!((written(copy), written(file)), (Some(copy), Some(file)) if copy > file)
}

/// Makes the definitions of an object, or of each object of an archive, weak.
fn weaken(data: &mut [u8]) {
    let members: Vec<Range<usize>> = match ArchiveFile::parse(&*data) {
        Ok(archive) => archive
            .members()
            .filter_map(|member| {
                let (start, len) = member.ok()?.file_range();
                Some(start as usize..(start + len) as usize)
            })
            .collect(),
        Err(_) => vec![0..data.len()],
    };
    for member in members {
        definitions(&mut data[member]);
    }
}

/// What becomes of a definition.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Made {
    /// It stays as it is.
    Kept,
    /// Weak, and still a name the program can export.
    Weak,
    /// Weak and private to the program.
    Private,
}

/// Makes the definitions of a Mach-O object weak, and its statics private to the program;
/// an object of another format stays as it is.
pub(crate) fn definitions(data: &mut [u8]) {
    weaken_but(data, |_| Made::Private);
    share_statics(data);
}

/// Makes each symbol of Rust's that is private to the object and names data the program
/// writes (a section `symbols::CopyOfStatic` reads as such) a private external, which the
/// code of another object can refer to: rustc's copy of a static it kept to its codegen unit,
/// which ra-jit's code has to share. Its place in the table stays, among the local ones,
/// which Apple's linker takes as it is, so no relocation's symbol changes; nor is it weak,
/// so that two of one name are an error, not one.
fn share_statics(data: &mut [u8]) {
    let written = sections_written(data);
    let Some((symbols, strings)) = symbol_table(data) else { return };
    let (Some(names), Some(_)) = (data.get(strings.clone()), data.get(symbols.clone())) else {
        return;
    };
    let names = names.to_vec();
    let Ok(symbols) = pod::slice_from_all_bytes_mut::<Nlist64<LE>>(&mut data[symbols]) else {
        return;
    };
    for symbol in symbols {
        let defined = symbol.n_type & (N_STAB | N_TYPE) == N_SECT;
        if !defined || symbol.n_type & N_EXT != 0 {
            continue;
        }
        if !written.get(usize::from(symbol.n_sect).wrapping_sub(1)).is_some_and(|it| *it) {
            continue;
        }
        let name = names.get(symbol.n_strx.get(LE) as usize..).unwrap_or_default();
        let name = name.split(|&it| it == 0).next().unwrap_or_default();
        let name = std::str::from_utf8(name).ok().and_then(|it| it.strip_prefix('_'));
        if name.is_some_and(|it| crate::symbols::rust_symbol_length(it).is_some()) {
            symbol.n_type |= N_EXT | N_PEXT;
        }
    }
}

/// Whether each section of a 64-bit Mach-O object, in the order symbols number them from 1,
/// holds data a program writes to, as `symbols::written_at_run_time` tells it.
fn sections_written(data: &[u8]) -> Vec<bool> {
    let mut written = Vec::new();
    let Ok(header) = MachHeader64::<LE>::parse(data, 0) else { return written };
    let Ok(mut commands) = header.load_commands(LE, data, 0) else { return written };
    while let Ok(Some(command)) = commands.next() {
        let Ok(Some((segment, sections))) = command.segment_64() else { continue };
        let Ok(sections) = segment.sections(LE, sections) else { return Vec::new() };
        for section in sections {
            let name = section.sectname.split(|&it| it == 0).next().unwrap_or_default();
            written.push(matches!(name, b"__data" | b"__bss" | b"__common" | b"__thread_vars"));
        }
    }
    written
}

/// As [`definitions`], for an object of ra-jit's. Of a program's whole, with a C `main` of
/// ra-jit's, that `main` and the functions a stand-in's allocator calls stay as they are: the
/// stand-ins rustc built have weak ones for cargo's link of them, and of two weak ones Apple's
/// linker may keep the stand-in's, which would start the program at whatever is laid out
/// after its `main`. A patch has no `main`, and gives way to the object it patches.
///
/// A name of C's (`#[no_mangle]`, `#[export_name]`) is weak and not private: a program can
/// export it (bun's `-exported_symbols_list` names its `napi_*` for the addons it loads), and
/// Apple's linker exports no private one. The names of Rust's and of ra-jit's are private.
pub(crate) fn ours(data: &mut [u8]) {
    let has_main = defined(data).iter().any(|it| it == b"_main");
    weaken_but(data, |name| {
        let name = name.strip_prefix(b"_").unwrap_or(name);
        let stays = name == b"main"
            || crate::stub::ALLOCATOR_FUNCTIONS.iter().any(|it| it.as_bytes() == name);
        match () {
            _ if has_main && stays => Made::Kept,
            _ if of_rust(name) => Made::Private,
            _ => Made::Weak,
        }
    });
}

/// Whether a name (without Mach-O's `_`) is a symbol rustc mangled or one of ra-jit's own.
fn of_rust(name: &[u8]) -> bool {
    let v0 = name
        .strip_prefix(b"_R")
        .and_then(|it| it.first())
        .is_some_and(|it| it.is_ascii_alphanumeric());
    name.starts_with(b"_ZN") || v0 || name.starts_with(b"ra-jit.")
}

/// The names an object defines and other objects can see.
fn defined(data: &[u8]) -> Vec<Vec<u8>> {
    let mut data = data.to_vec();
    let mut names = Vec::new();
    weaken_but(&mut data, |name| {
        names.push(name.to_vec());
        Made::Kept
    });
    names
}

fn weaken_but(data: &mut [u8], mut made: impl FnMut(&[u8]) -> Made) {
    let Some((symbols, strings)) = symbol_table(data) else { return };
    let (Some(names), Some(_)) = (data.get(strings.clone()), data.get(symbols.clone())) else {
        return;
    };
    let names = names.to_vec();
    let Ok(symbols) = pod::slice_from_all_bytes_mut::<Nlist64<LE>>(&mut data[symbols]) else {
        return;
    };
    for symbol in symbols {
        let defined = symbol.n_type & (N_STAB | N_TYPE) == N_SECT;
        if !defined || symbol.n_type & N_EXT == 0 {
            continue;
        }
        let name = names.get(symbol.n_strx.get(LE) as usize..).unwrap_or_default();
        let name = name.split(|&it| it == 0).next().unwrap_or_default();
        let made = made(name);
        if made == Made::Private {
            symbol.n_type |= N_PEXT;
        }
        if made != Made::Kept {
            symbol.n_desc.set(LE, symbol.n_desc(LE) | N_WEAK_DEF);
        }
    }
}

/// Where in a 64-bit Mach-O object its symbols are, and their names.
fn symbol_table(data: &[u8]) -> Option<(Range<usize>, Range<usize>)> {
    let header = MachHeader64::<LE>::parse(data, 0).ok()?;
    let mut commands = header.load_commands(LE, data, 0).ok()?;
    while let Ok(Some(command)) = commands.next() {
        if command.cmd() != LC_SYMTAB {
            continue;
        }
        let symtab = command.symtab().ok()??;
        let start = symtab.symoff.get(LE) as usize;
        let len = symtab.nsyms.get(LE) as usize * size_of::<Nlist64<LE>>();
        let strings = symtab.stroff.get(LE) as usize;
        let strings = strings..strings + symtab.strsize.get(LE) as usize;
        return Some((start..start + len, strings));
    }
    None
}

#[cfg(test)]
mod tests {
    use object::write::{Mangling, Object, Symbol, SymbolSection};
    use object::{
        Architecture, BinaryFormat, Endianness, SectionKind, SymbolFlags, SymbolKind, SymbolScope,
    };
    use object::{Object as _, ObjectSymbol as _};

    use super::*;

    const NAMES: [&str; 6] = [
        "_main",
        "_napi_create_async_work",
        "___ra_jit_global_alloc",
        "__ZN3app4work17h0123456789abcdefE",
        "__RNvCs1234_3app4work",
        "_ra-jit.0123456789abcdef.app::work",
    ];

    fn object() -> Vec<u8> {
        let mut object =
            Object::new(BinaryFormat::MachO, Architecture::Aarch64, Endianness::Little);
        object.set_mangling(Mangling::None);
        let text = object.add_section(Vec::new(), b"__text".to_vec(), SectionKind::Text);
        for (at, name) in NAMES.iter().enumerate() {
            let offset = object.append_section_data(text, &[0; 4], 4);
            assert_eq!(offset, 4 * at as u64);
            object.add_symbol(Symbol {
                name: name.as_bytes().to_vec(),
                value: offset,
                size: 4,
                kind: SymbolKind::Text,
                scope: SymbolScope::Dynamic,
                weak: false,
                section: SymbolSection::Section(text),
                flags: SymbolFlags::None,
            });
        }
        object.write().unwrap()
    }

    /// Whether each of [`NAMES`] is private and whether it is weak.
    fn made(data: &[u8]) -> Vec<(bool, bool)> {
        let (symbols, strings) = symbol_table(data).unwrap();
        let symbols = pod::slice_from_all_bytes::<Nlist64<LE>>(&data[symbols]).unwrap();
        let named = |name: &str| {
            let symbol = symbols.iter().find(|it| {
                let at = strings.start + it.n_strx.get(LE) as usize;
                data[at..].split(|&it| it == 0).next() == Some(name.as_bytes())
            });
            let symbol = symbol.unwrap();
            (symbol.n_type & N_PEXT != 0, symbol.n_desc(LE) & N_WEAK_DEF != 0)
        };
        NAMES.iter().map(|it| named(it)).collect()
    }

    #[test]
    fn a_name_of_cs_in_ours_can_still_be_exported() {
        let mut data = object();
        ours(&mut data);
        let (private, weak) = (true, true);
        let (kept, exported, mangled) = ((!private, !weak), (!private, weak), (private, weak));
        assert_eq!(made(&data), [kept, exported, kept, mangled, mangled, mangled]);
        let mut data = object();
        definitions(&mut data);
        assert!(made(&data).iter().all(|&it| it == (private, weak)));
    }

    /// rustc's copies of statics it kept to their object, which LLVM may have renamed, become
    /// linkable; the memory a static points to, a thread-local's first value, and what is
    /// not written at run time stay as they are.
    #[test]
    fn a_static_private_to_rustcs_object_is_made_linkable() {
        let mut object =
            Object::new(BinaryFormat::MachO, Architecture::Aarch64, Endianness::Little);
        object.set_mangling(Mangling::None);
        let data = object.add_section(b"__DATA".to_vec(), b"__data".to_vec(), SectionKind::Data);
        let constant =
            object.add_section(b"__TEXT".to_vec(), b"__const".to_vec(), SectionKind::ReadOnlyData);
        let symbols = [
            ("__RNvCs1234_3app7SERVING.0", data, true),
            ("__RNvCs1234_3app6ALLOCS", data, true),
            ("__RNvCs1234_3app6ALLOCS.0.llvm.123", data, true),
            ("__ZN3app6ALLOCS17h0123456789abcdefE.0", data, true),
            ("__RNnCs1234_3app6ALLOCS", data, false),
            ("__RNvCs1234_3app5TABLE", constant, false),
            ("_counter", data, false),
        ];
        for (name, section, _) in symbols {
            let value = object.append_section_data(section, &[0; 8], 8);
            object.add_symbol(Symbol {
                name: name.as_bytes().to_vec(),
                value,
                size: 8,
                kind: SymbolKind::Data,
                scope: SymbolScope::Compilation,
                weak: false,
                section: SymbolSection::Section(section),
                flags: SymbolFlags::None,
            });
        }
        let mut data = object.write().unwrap();
        definitions(&mut data);
        let read = object::File::parse(&*data).unwrap();
        for (name, _, linkable) in symbols {
            let symbol = read.symbols().find(|it| it.name() == Ok(name)).unwrap();
            assert_eq!(symbol.is_global(), linkable, "{name}");
        }
    }
}
