use std::env;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub input: String,
    pub output: Option<String>,
    pub columns: Vec<usize>,
    pub delimiter: char,
    pub mask_char: char,
    pub keep_header: bool,
    pub keep_start: Option<usize>,
    pub keep_end: Option<usize>,
    pub threads: usize,
}

pub enum CliAction {
    Run(Args),
    Help,
    Version,
}

pub fn help_text() -> &'static str {
    "\
csvmask - High-performance CSV column masking tool

USAGE:
    csvmask [OPTIONS] <INPUT> <COLUMNS>
    csvmask -i <INPUT> -c <COLUMNS> [OPTIONS]

ARGS:
    <INPUT>      Input CSV file path (use '-' for stdin)
    <COLUMNS>    1-based column indices to mask (e.g. '1,3,5' or '1,3-5')

OPTIONS:
    -i, --input <FILE>          Input CSV file path (use '-' for stdin)
    -c, --columns <COLS>        1-based column indices to mask (repeatable)
    -o, --output <FILE>         Output CSV file path (defaults to stdout)
    -d, --delimiter <CHAR>      CSV field separator [default: ;] (supports '\\t' or 'tab')
    -s, --separator <CHAR>      Alias for --delimiter
    -m, --mask-char <CHAR>      Masking character [default: *]
    -H, --keep-header           Preserve the first row (header) unmasked
        --keep-start <N>        Leading characters to keep unmasked in masked fields
        --keep-end <N>          Trailing characters to keep unmasked in masked fields
    -j, --threads <N>           Number of worker threads [default: auto]
    -h, --help                  Print help information
    -V, --version               Print version information

EXAMPLES:
    csvmask users.csv 2,4 -o users_masked.csv
    csvmask users.csv 1,3-5 -d ',' -m '#' -H -o out.csv
    cat users.csv | csvmask - 2 > masked.csv
"
}

fn parse_single_char(s: &str, flag: &str, allow_tab_alias: bool) -> Result<char, String> {
    if allow_tab_alias && (s == "\\t" || s.eq_ignore_ascii_case("tab")) {
        return Ok('\t');
    }
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Ok(c),
        _ => Err(format!("invalid value '{s}' for {flag}: expected a single character")),
    }
}

pub fn parse_columns_spec(spec: &str, out: &mut Vec<usize>) -> Result<(), String> {
    if spec.trim().is_empty() {
        return Err("column list cannot be empty".to_string());
    }
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("invalid empty entry in column list '{spec}'"));
        }
        if let Some((start_s, end_s)) = part.split_once('-') {
            let start: usize = start_s
                .trim()
                .parse()
                .map_err(|_| format!("invalid column number '{start_s}' in range '{part}'"))?;
            let end: usize = end_s
                .trim()
                .parse()
                .map_err(|_| format!("invalid column number '{end_s}' in range '{part}'"))?;
            if start == 0 || end == 0 {
                return Err("column indices are 1-based and must be >= 1".to_string());
            }
            if start > end {
                return Err(format!("invalid column range '{part}': start > end"));
            }
            for c in start..=end {
                out.push(c);
            }
        } else {
            let col: usize = part
                .parse()
                .map_err(|_| format!("invalid column number '{part}'"))?;
            if col == 0 {
                return Err("column indices are 1-based and must be >= 1".to_string());
            }
            out.push(col);
        }
    }
    Ok(())
}

pub fn parse_args() -> Result<CliAction, String> {
    parse_args_from(env::args().skip(1))
}

pub fn parse_args_from<I: IntoIterator<Item = String>>(iter: I) -> Result<CliAction, String> {
    let mut input: Option<String> = None;
    let mut output: Option<String> = None;
    let mut columns: Vec<usize> = Vec::new();
    let mut delimiter: char = ';';
    let mut mask_char: char = '*';
    let mut keep_header = false;
    let mut keep_start: Option<usize> = None;
    let mut keep_end: Option<usize> = None;
    let mut threads: Option<usize> = None;
    let mut positional: Vec<String> = Vec::new();

    let mut it = iter.into_iter();
    while let Some(arg) = it.next() {
        if arg == "--" {
            positional.extend(it);
            break;
        }
        match arg.as_str() {
            "-h" | "--help" => return Ok(CliAction::Help),
            "-V" | "--version" => return Ok(CliAction::Version),
            "-H" | "--keep-header" => keep_header = true,
            "-i" | "--input" => {
                let val = it.next().ok_or("missing value for --input")?;
                input = Some(val);
            }
            "-o" | "--output" => {
                let val = it.next().ok_or("missing value for --output")?;
                output = Some(val);
            }
            "-c" | "--columns" => {
                let val = it.next().ok_or("missing value for --columns")?;
                parse_columns_spec(&val, &mut columns)?;
            }
            "-d" | "--delimiter" | "-s" | "--separator" => {
                let val = it.next().ok_or("missing value for --delimiter")?;
                delimiter = parse_single_char(&val, "--delimiter", true)?;
            }
            "-m" | "--mask-char" | "--mask" => {
                let val = it.next().ok_or("missing value for --mask-char")?;
                mask_char = parse_single_char(&val, "--mask-char", false)?;
            }
            "--keep-start" => {
                let val = it.next().ok_or("missing value for --keep-start")?;
                let n = val
                    .parse::<usize>()
                    .map_err(|_| format!("invalid integer '{val}' for --keep-start"))?;
                keep_start = Some(n);
            }
            "--keep-end" => {
                let val = it.next().ok_or("missing value for --keep-end")?;
                let n = val
                    .parse::<usize>()
                    .map_err(|_| format!("invalid integer '{val}' for --keep-end"))?;
                keep_end = Some(n);
            }
            "-j" | "--threads" => {
                let val = it.next().ok_or("missing value for --threads")?;
                let n = val
                    .parse::<usize>()
                    .map_err(|_| format!("invalid integer '{val}' for --threads"))?;
                if n == 0 {
                    return Err("--threads must be >= 1".to_string());
                }
                threads = Some(n);
            }
            _ if arg.starts_with("--delimiter=") => {
                delimiter = parse_single_char(&arg["--delimiter=".len()..], "--delimiter", true)?;
            }
            _ if arg.starts_with("--separator=") => {
                delimiter = parse_single_char(&arg["--separator=".len()..], "--separator", true)?;
            }
            _ if arg.starts_with("--mask-char=") => {
                mask_char = parse_single_char(&arg["--mask-char=".len()..], "--mask-char", false)?;
            }
            _ if arg.starts_with("--mask=") => {
                mask_char = parse_single_char(&arg["--mask=".len()..], "--mask", false)?;
            }
            _ if arg.starts_with("--input=") => {
                input = Some(arg["--input=".len()..].to_string());
            }
            _ if arg.starts_with("--output=") => {
                output = Some(arg["--output=".len()..].to_string());
            }
            _ if arg.starts_with("--columns=") => {
                parse_columns_spec(&arg["--columns=".len()..], &mut columns)?;
            }
            _ if arg.starts_with("--keep-start=") => {
                let val = &arg["--keep-start=".len()..];
                keep_start = Some(
                    val.parse()
                        .map_err(|_| format!("invalid integer '{val}' for --keep-start"))?,
                );
            }
            _ if arg.starts_with("--keep-end=") => {
                let val = &arg["--keep-end=".len()..];
                keep_end = Some(
                    val.parse()
                        .map_err(|_| format!("invalid integer '{val}' for --keep-end"))?,
                );
            }
            _ if arg.starts_with("--threads=") => {
                let val = &arg["--threads=".len()..];
                let n: usize = val
                    .parse()
                    .map_err(|_| format!("invalid integer '{val}' for --threads"))?;
                if n == 0 {
                    return Err("--threads must be >= 1".to_string());
                }
                threads = Some(n);
            }
            _ if arg.starts_with('-') && arg.len() > 1 => {
                return Err(format!("unknown option '{arg}' (use --help for usage)"));
            }
            _ => positional.push(arg),
        }
    }

    let mut pos_iter = positional.into_iter();
    if input.is_none() {
        input = pos_iter.next();
    }
    if columns.is_empty()
        && let Some(col_arg) = pos_iter.next()
    {
        parse_columns_spec(&col_arg, &mut columns)?;
    }
    if let Some(extra) = pos_iter.next() {
        return Err(format!(
            "unexpected argument '{extra}' (use -o/--output for output file)"
        ));
    }

    let input = input.ok_or("missing input CSV file (use --help for usage)")?;
    if columns.is_empty() {
        return Err("missing columns list (e.g. '1,3'; use --help for usage)".to_string());
    }

    let default_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    Ok(CliAction::Run(Args {
        input,
        output,
        columns,
        delimiter,
        mask_char,
        keep_header,
        keep_start,
        keep_end,
        threads: threads.unwrap_or(default_threads),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<CliAction, String> {
        parse_args_from(args.iter().map(|s| (*s).to_string()))
    }

    #[test]
    fn test_positional_defaults() {
        let CliAction::Run(a) = parse(&["data.csv", "1,3-5"]).unwrap() else {
            panic!("expected Run");
        };
        assert_eq!(a.input, "data.csv");
        assert_eq!(a.columns, vec![1, 3, 4, 5]);
        assert_eq!(a.delimiter, ';');
        assert_eq!(a.mask_char, '*');
        assert_eq!(a.output, None);
        assert!(!a.keep_header);
    }

    #[test]
    fn test_flags_and_tab_delimiter() {
        let CliAction::Run(a) = parse(&[
            "-i", "in.tsv", "-c", "2,4", "-d", "tab", "-m", "#", "-H", "-o", "out.tsv", "-j", "2",
        ])
        .unwrap() else {
            panic!("expected Run");
        };
        assert_eq!(a.input, "in.tsv");
        assert_eq!(a.output.as_deref(), Some("out.tsv"));
        assert_eq!(a.columns, vec![2, 4]);
        assert_eq!(a.delimiter, '\t');
        assert_eq!(a.mask_char, '#');
        assert!(a.keep_header);
        assert_eq!(a.threads, 2);
    }

    #[test]
    fn test_zero_column_rejected() {
        assert!(parse(&["data.csv", "0,1"]).is_err());
    }
}
