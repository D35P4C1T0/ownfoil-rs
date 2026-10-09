# nx-archive

nx-archive is a Rust library for working with various Nintendo archive formats.

It provides an idiomatic Rust interface, and helper traits and functions for working with the various formats.

nx-archive supports the following formats:

- NCA (Nintendo Content Archive) (NCA3 Only at the moment)
- NSP (Nintendo Switch Package) and PFS0 (Partition File System 0)
- XCI (Nintendo Switch Game Card Image) (Incomplete, extracts files but does not parse the entire format)
- CNMT (Packaged Content Meta Table)
- RomFS (Read-Only File System)

It plans to support all other Nintendo archive formats in the future, including but not limited to:

- NACP (Nintendo Application Control Property)
- NAX0 (AEX-XTS SD card filesystem)
- NSO (Nintendo Switch Object)
- ExeFS (Executable File System)
- Older NCAs (NCA0, NCA1, NCA2)
- NRO (Nintendo Switch Executable)
- NRR (Nintendo Switch executable verification data)
- IMKV (Key-value pair file format)
- NPDM (Extended headers)

## Usage

After adding nx-archive to your `Cargo.toml`, you can use it like so:

```rust no_run
use nx_archive::formats::pfs0::Pfs0;
use nx_archive::formats::nca::Nca;
use nx_archive::formats::hfs0::Hfs0;
use std::fs::File;
fn main() {
    let nsp_file = File::open("path/to/file.nsp").unwrap();
    let pfs0 = Pfs0::from_reader(nsp_file).unwrap();

    for file in pfs0.list_files().unwrap() {
        println!("File: {}", file.name);
    }
}
```
