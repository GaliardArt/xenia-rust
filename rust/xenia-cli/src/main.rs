use std::env;
use std::error::Error;
use std::fs;
use std::io::{self, ErrorKind};

use xenia_core::{XEX_HEADER_ENTRY_POINT, parse_xex2_header};

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
            println!("  xenia-rust --version");
            println!("  xenia-rust --help");
            println!();
            println!(
                "The current build can inspect XEX2 header metadata only; it cannot run games."
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
