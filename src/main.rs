extern crate byteorder;
extern crate xmas_elf;
extern crate toml;
extern crate cargo_metadata;

use std::io;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use byteorder::{ByteOrder, LittleEndian};
use args::Args;
use cargo_metadata::Metadata as CargoMetadata;
use cargo_metadata::Package as CrateMetadata;

mod args;

const BLOCK_SIZE: usize = 512;
type KernelInfoBlock = [u8; BLOCK_SIZE];

pub fn main() {
    if let Err(err) = run() {
        panic!("Error: {:?}", err);
    }
}

#[derive(Debug)]
enum Error {
    Io(io::Error),
    CargoMetadata(cargo_metadata::Error),
    Bootloader(String, io::Error),
}

impl From<io::Error> for Error {
    fn from(other: io::Error) -> Self {
        Error::Io(other)
    }
}

impl From<cargo_metadata::Error> for Error {
    fn from(other: cargo_metadata::Error) -> Self {
        Error::CargoMetadata(other)
    }
}

fn run() -> Result<(), Error> {
    let args = args::args();

    let metadata = read_cargo_metadata(&args)?;

    let (kernel, out_dir) = build_kernel(&args, &metadata)?;

    let kernel_size = kernel.metadata()?.len();
    let kernel_info_block = create_kernel_info_block(kernel_size);

    let bootloader = build_bootloader(&out_dir)?;

    create_disk_image(&args, kernel, kernel_info_block, &bootloader)?;

    Ok(())
}

fn read_cargo_metadata(args: &Args) -> Result<CargoMetadata, cargo_metadata::Error> {
    cargo_metadata::metadata(args.manifest_path.as_ref().map(PathBuf::as_path))
}

fn build_kernel(args: &args::Args, metadata: &CargoMetadata) -> Result<(File, PathBuf), Error> {
    let crate_root = PathBuf::from(&metadata.workspace_root);
    let manifest_path = args.manifest_path.as_ref().map(Clone::clone).unwrap_or({
        let mut path = crate_root.clone();
        path.push("Cargo.toml");
        path
    });
    let crate_ = metadata.packages.iter().find(|p| Path::new(&p.manifest_path) == manifest_path)
        .expect("Could not read crate name from cargo metadata");
    let crate_name = &crate_.name;

    let target_dir = PathBuf::from(&metadata.target_directory);

    // compile kernel
    println!("Building kernel");
    let exit_status = run_xargo_build(&std::env::current_dir()?, &args.all_cargo)?;
    if !exit_status.success() { std::process::exit(1) }

    let mut out_dir = target_dir;
    if let &Some(ref target) = &args.target {
        out_dir.push(target);
    }
    if args.release {
        out_dir.push("release");
    } else {
        out_dir.push("debug");
    }

    let mut kernel_path = out_dir.clone();
    kernel_path.push(crate_name);
    let kernel = File::open(kernel_path)?;
    Ok((kernel, out_dir))
}

fn run_xargo_build(pwd: &Path, args: &[String]) -> io::Result<std::process::ExitStatus> {
    let mut command = Command::new("xargo");
    command.arg("build");
    command.current_dir(pwd).env("RUST_TARGET_PATH", pwd);
    command.args(args);
    command.status()
}

fn create_kernel_info_block(kernel_size: u64) -> KernelInfoBlock {
    let kernel_size = if kernel_size <= u64::from(u32::max_value()) {
        kernel_size as u32
    } else {
        panic!("Kernel can't be loaded by BIOS bootloader because is too big")
    };

    let mut kernel_info_block = [0u8; BLOCK_SIZE];
    LittleEndian::write_u32(&mut kernel_info_block[0..4], kernel_size);

    kernel_info_block
}

fn download_bootloader(out_dir: &Path) -> Result<CrateMetadata, Error> {
    use std::io::Write;

    let bootloader_dir = {
        let mut dir = PathBuf::from(out_dir);
        dir.push("bootloader");
        dir
    };

    let cargo_toml = {
        let mut dir = bootloader_dir.clone();
        dir.push("Cargo.toml");
        dir
    };
    let src_lib = {
        let mut dir = bootloader_dir.clone();
        dir.push("src");
        fs::create_dir_all(dir.as_path())?;
        dir.push("lib.rs");
        dir
    };

    File::create(&cargo_toml)?.write_all(r#"
        [package]
        authors = ["author@example.com>"]
        name = "bootloader_download_helper"
        version = "0.0.0"

        [dependencies.bootloader]
        git = "https://github.com/rust-osdev/bootloader.git"
    "#.as_bytes())?;

    File::create(src_lib)?.write_all(r#"
        #![no_std]
    "#.as_bytes())?;

    let mut command = Command::new("cargo");
    command.arg("fetch");
    command.current_dir(bootloader_dir);
    assert!(command.status()?.success(), "Bootloader download failed.");

    let metadata = cargo_metadata::metadata_deps(Some(&cargo_toml), true)?;
    let bootloader = metadata.packages.iter().find(|p| p.name == "bootloader")
        .expect("Could not find crate named “bootloader”");

    Ok(bootloader.clone())
}

fn build_bootloader(out_dir: &Path) -> Result<Box<[u8]>, Error> {
    use std::io::Read;

    let bootloader_metadata = download_bootloader(out_dir)?;
    let bootloader_dir = Path::new(&bootloader_metadata.manifest_path).parent().unwrap();

    let bootloader_target = "x86_64-bootloader";
    let mut bootloader_path = bootloader_dir.to_path_buf();
    bootloader_path.push("bootloader.bin");

    let args = &[
        String::from("--target"),
        String::from(bootloader_target),
        String::from("--release"),
    ];

    println!("Building bootloader");
    let exit_status = run_xargo_build(bootloader_dir, args)?;
    if !exit_status.success() { std::process::exit(1) }

    let mut bootloader_elf_path = bootloader_dir.to_path_buf();
    bootloader_elf_path.push("target");
    bootloader_elf_path.push(bootloader_target);
    bootloader_elf_path.push("release/bootloader");

    let mut bootloader_elf_bytes = Vec::new();
    let mut bootloader = File::open(&bootloader_elf_path).map_err(|err| {
        Error::Bootloader(format!("Could not open bootloader at {:?}", bootloader_elf_path), err)
    })?;
    bootloader.read_to_end(&mut bootloader_elf_bytes)?;

    // copy bootloader section of ELF file to bootloader_path
    let elf_file = xmas_elf::ElfFile::new(&bootloader_elf_bytes).unwrap();
    xmas_elf::header::sanity_check(&elf_file).unwrap();
    let bootloader_section = elf_file.find_section_by_name(".bootloader")
        .expect("bootloader must have a .bootloader section");

    Ok(Vec::from(bootloader_section.raw_data(&elf_file)).into_boxed_slice())
}

fn create_disk_image(args: &Args, mut kernel: File, kernel_info_block: KernelInfoBlock,
    bootloader_data: &[u8]) -> Result<(), Error>
{
    use std::io::{Read, Write};

    println!("Creating disk image at {:?}", args.output);
    let mut output = File::create(&args.output)?;
    output.write_all(&bootloader_data)?;
    output.write_all(&kernel_info_block)?;

    // write out kernel elf file
    let kernel_size = kernel.metadata()?.len();
    let mut buffer = [0u8; 1024];
    loop {
        let (n, interrupted) = match kernel.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => (n, false),
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => (0, true),
            Err(e) => Err(e)?,
        };
        if !interrupted {
            output.write_all(&buffer[..n])?
        }
    }

    let padding_size = ((512 - (kernel_size % 512)) % 512) as usize;
    let padding = [0u8; 512];
    output.write_all(&padding[..padding_size])?;

    Ok(())
}
