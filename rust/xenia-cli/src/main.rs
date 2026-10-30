use std::env;
use std::error::Error;
use std::fs;
use std::io::{self, ErrorKind};

use xenia_core::{
    XEX_HEADER_ENTRY_POINT, load_xex2_image, parse_xbox_pe32, parse_xex2_header,
    parse_xex2_security_info,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("xenia-rust: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("inspect-xex") => {
            let path = args
                .next()
                .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "missing XEX file path"))?;
            if args.next().is_some() {
                return Err(
                    io::Error::new(ErrorKind::InvalidInput, "unexpected extra arguments").into(),
                );
            }

            let bytes = fs::read(&path)?;
            let header = parse_xex2_header(&bytes)?;
            println!("XEX2 header: {path}");
            println!("  module flags:        0x{:08X}", header.module_flags);
            println!("  header size:         0x{:08X}", header.header_size);
            println!("  reserved:            0x{:08X}", header.reserved);
            println!(
                "  security info offset: 0x{:08X}",
                header.security_info_offset
            );
            println!("  optional headers:    {}", header.optional_headers.len());
            match parse_xex2_security_info(&bytes, &header) {
                Ok(security) => {
                    println!("  image size:          0x{:08X}", security.image_size);
                    println!("  load address:        0x{:08X}", security.load_address);
                    println!("  image flags:         0x{:08X}", security.image_flags);
                    println!("  page descriptors:    {}", security.page_descriptors.len());
                }
                Err(error) => println!("  security info:       unavailable ({error})"),
            }
            match load_xex2_image(&bytes) {
                Ok(image) => {
                    println!("  load profile:        unencrypted/uncompressed");
                    println!("  mapped base:         0x{:08X}", image.layout.load_base);
                    println!("  entry point:         0x{:08X}", image.layout.entry_point);
                    println!("  PE sections:         {}", image.layout.sections.len());
                }
                Err(error) => println!("  image load profile:  not available ({error})"),
            }
            if let Some(entry_point) = header.optional_header_value(XEX_HEADER_ENTRY_POINT) {
                println!("  entry point:         0x{entry_point:08X}");
            }
            for (index, optional) in header.optional_headers.iter().enumerate() {
                println!(
                    "    [{index:03}] key=0x{:08X} value=0x{:08X}",
                    optional.key, optional.value
                );
            }
            Ok(())
        }
        Some("inspect-pe") => {
            let path = args
                .next()
                .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "missing PE image path"))?;
            if args.next().is_some() {
                return Err(
                    io::Error::new(ErrorKind::InvalidInput, "unexpected extra arguments").into(),
                );
            }

            let bytes = fs::read(&path)?;
            let image = parse_xbox_pe32(&bytes)?;
            println!("Xbox 360 PE32 image: {path}");
            println!("  image base:          0x{:08X}", image.image_base);
            println!("  entry-point RVA:     0x{:08X}", image.entry_point_rva);
            println!("  subsystem:           {}", image.subsystem);
            println!("  sections:             {}", image.sections.len());
            for (index, section) in image.sections.iter().enumerate() {
                let name = String::from_utf8_lossy(&section.name);
                println!(
                    "    [{index:03}] {} RVA=0x{:08X} size=0x{:X} flags=0x{:08X}",
                    name.trim_end_matches(char::from(0)),
                    section.virtual_address,
                    section.virtual_size,
                    section.characteristics
                );
            }
            Ok(())
        }
        Some("--version") | Some("-V") => {
            println!("xenia-rust {}", env!("CARGO_PKG_VERSION"));
            println!("experimental Rust rewrite; game execution is not implemented");
            Ok(())
        }
        Some("--help") | Some("-h") | None => {
            println!("Xenia Rust rewrite — experimental diagnostic tools");
            println!();
            println!("Usage:");
            println!("  xenia-rust inspect-xex <file.xex>");
            println!("  xenia-rust inspect-pe <decompressed-image.bin>");
            println!("  xenia-rust --version");
            println!("  xenia-rust --help");
            println!();
            println!(
                "The current build can inspect XEX2 and Xbox PE32 metadata; it cannot run games."
            );
            Ok(())
        }
        Some(command) => Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("unknown command '{command}'; use --help"),
        )
        .into()),
    }
}
