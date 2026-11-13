mod cli;
mod mask;
mod processor;
mod swar;

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::process;

use cli::{CliAction, help_text, parse_args};
use mask::MaskConfig;
use processor::{ColumnSelector, ProcessOptions, run};

const IO_BUF_SIZE: usize = 1024 * 1024;

fn is_same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

fn execute() -> Result<(), Box<dyn std::error::Error>> {
    let args = match parse_args() {
        Ok(CliAction::Run(a)) => a,
        Ok(CliAction::Help) => {
            print!("{}", help_text());
            return Ok(());
        }
        Ok(CliAction::Version) => {
            println!("csvmask {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Err(msg) => {
            return Err(msg.into());
        }
    };

    if let Some(out_path) = &args.output
        && args.input != "-"
        && out_path != "-"
        && is_same_file(Path::new(&args.input), Path::new(out_path))
    {
        return Err("output path must not be the same as input path".into());
    }

    let cols = ColumnSelector::from_one_based(&args.columns);
    let cfg = MaskConfig::new(
        args.mask_char,
        args.delimiter,
        args.keep_start,
        args.keep_end,
    );

    let (reader, size_hint): (Box<dyn Read + Send>, Option<u64>) = if args.input == "-" {
        (Box::new(io::stdin()), None)
    } else {
        let file = File::open(&args.input)
            .map_err(|e| format!("failed to open input '{}': {e}", args.input))?;
        let size = file.metadata().ok().map(|m| m.len());
        (Box::new(file), size)
    };

    let writer: Box<dyn Write> = match args.output.as_deref() {
        None | Some("-") => Box::new(BufWriter::with_capacity(IO_BUF_SIZE, io::stdout())),
        Some(path) => {
            let file = File::create(path)
                .map_err(|e| format!("failed to create output '{}': {e}", path))?;
            Box::new(BufWriter::with_capacity(IO_BUF_SIZE, file))
        }
    };

    let opts = ProcessOptions {
        keep_header: args.keep_header,
        threads: args.threads,
        input_size_hint: size_hint,
    };

    if let Err(err) = run(reader, writer, &cols, &cfg, &opts) {
        if err.kind() == io::ErrorKind::BrokenPipe {
            return Ok(());
        }
        return Err(err.into());
    }

    Ok(())
}

fn main() {
    if let Err(err) = execute() {
        eprintln!("csvmask: {err}");
        process::exit(1);
    }
}
