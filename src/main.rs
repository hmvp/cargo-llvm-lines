use std::collections::HashMap as Map;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Write, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::{self, Child, Stdio, Command};

extern crate isatty;
use isatty::stderr_isatty;

extern crate rustc_demangle;
use rustc_demangle::demangle;

extern crate tempdir;
use tempdir::TempDir;

fn main() {
    let result = cargo_llvm_lines();
    process::exit(match result {
        Ok(code) => code,
        Err(err) => {
            let _ = writeln!(&mut io::stderr(), "{}", err);
            1
        }
    });
}

fn cargo_llvm_lines() -> io::Result<i32> {
    match env::args_os().last().unwrap().to_str().unwrap_or("") {
        "--filter-cargo" => filter_err(ignore_cargo_err),
        _ => {}
    }

    let outdir = TempDir::new("cargo-llvm-lines").expect("failed to create tmp file");
    let outfile = outdir.path().join("crate");

    run_cargo_rustc(outfile)?;
    let ir = read_llvm_ir(outdir)?;
    count_lines(ir);

    Ok(0)
}

fn run_cargo_rustc(outfile: PathBuf) -> io::Result<()> {
    let mut cmd = Command::new("cargo");
    let args: Vec<_> = env::args_os().collect();
    cmd.args(&wrap_args(args.clone(), outfile.as_ref()));

    let mut filter_cargo = Vec::new();
    filter_cargo.extend(args.iter().map(OsString::as_os_str));
    filter_cargo.push(OsStr::new("--filter-cargo"));

    let _wait = cmd.pipe_to(&[OsStr::new("cat")], &filter_cargo)?;
    run(cmd)?;
    drop(_wait);

    Ok(())
}

fn read_llvm_ir(outdir: TempDir) -> io::Result<String> {
    for file in fs::read_dir(&outdir)? {
        let path = file?.path();
        if let Some(ext) = path.extension() {
            if ext == "ll" {
                let mut content = String::new();
                File::open(&path)?.read_to_string(&mut content)?;
                return Ok(content);
            }
        }
    }

    let msg = "Ran --emit=llvm-ir but did not find output IR";
    Err(io::Error::new(ErrorKind::Other, msg))
}

#[derive(Default)]
struct Instantiations {
    copies: usize,
    total_lines: usize,
}

impl Instantiations {
    fn record_lines(&mut self, lines: usize) {
        self.copies += 1;
        self.total_lines += lines;
    }
}

fn count_lines(content: String) {
    let mut instantiations = Map::<String, Instantiations>::new();
    let mut current_function = None;
    let mut count = 0;

    for line in content.lines() {
        if line.starts_with("define ") {
            current_function = parse_function_name(line);
        } else if line == "}" {
            if let Some(name) = current_function.take() {
                instantiations.entry(name)
                    .or_insert_with(Default::default)
                    .record_lines(count);
            }
            count = 0;
        } else if line.starts_with("  ") && !line.starts_with("   ") {
            count += 1;
        }
    }

    let mut data = instantiations.into_iter().collect::<Vec<_>>();
    data.sort_by(|a, b| {
        let key_lo = (b.1.total_lines, b.1.copies, &a.0);
        let key_hi = (a.1.total_lines, a.1.copies, &b.0);
        key_lo.cmp(&key_hi)
    });

    let stdout = io::stdout();
    let mut handle = stdout.lock();
    for row in data {
        let _ = writeln!(handle, "{:7} {:4}  {}", row.1.total_lines, row.1.copies, row.0);
    }
}

fn parse_function_name(line: &str) -> Option<String> {
    let start = line.find('@')? + 1;
    let end = line[start..].find('(')?;
    let mangled = line[start..start + end].trim_matches('"');
    let mut name = demangle(mangled).to_string();
    if has_hash(&name) {
        let len = name.len() - 19;
        name.truncate(len);
    }
    Some(name)
}

fn has_hash(name: &str) -> bool {
    let mut bytes = name.bytes().rev();
    for _ in 0..16 {
        if !bytes.next().map(is_ascii_hexdigit).unwrap_or(false) {
            return false;
        }
    }
    bytes.next() == Some(b'h')
        && bytes.next() == Some(b':')
        && bytes.next() == Some(b':')
}

fn is_ascii_hexdigit(byte: u8) -> bool {
    byte >= b'0' && byte <= b'9' || byte >= b'a' && byte <= b'f'
}

fn run(mut cmd: Command) -> io::Result<i32> {
    cmd.status().map(|status| status.code().unwrap_or(1))
}

struct Wait(Vec<Child>);

impl Drop for Wait {
    fn drop(&mut self) {
        for child in &mut self.0 {
            if let Err(err) = child.wait() {
                let _ = writeln!(&mut io::stderr(), "{}", err);
            }
        }
    }
}

trait PipeTo {
    fn pipe_to(&mut self, out: &[&OsStr], err: &[&OsStr]) -> io::Result<Wait>;
}

impl PipeTo for Command {
    fn pipe_to(&mut self, out: &[&OsStr], err: &[&OsStr]) -> io::Result<Wait> {
        use std::os::unix::io::{AsRawFd, FromRawFd};

        self.stdout(Stdio::piped());
        self.stderr(Stdio::piped());

        let child = self.spawn()?;

        *self = Command::new(out[0]);
        self.args(&out[1..]);
        self.stdin(unsafe {
            Stdio::from_raw_fd(child.stdout.as_ref().map(AsRawFd::as_raw_fd).unwrap())
        });

        let mut errcmd = Command::new(err[0]);
        errcmd.args(&err[1..]);
        errcmd.stdin(unsafe {
            Stdio::from_raw_fd(child.stderr.as_ref().map(AsRawFd::as_raw_fd).unwrap())
        });
        errcmd.stdout(Stdio::null());
        errcmd.stderr(Stdio::inherit());
        let spawn = errcmd.spawn()?;
        Ok(Wait(vec![spawn, child]))
    }
}

// Based on https://github.com/rsolomo/cargo-check
fn wrap_args<I>(it: I, outfile: &Path) -> Vec<OsString>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = vec!["rustc".into()];
    let mut has_color = false;

    let mut it = it.into_iter().skip(2);
    for arg in &mut it {
        if arg == *"--" {
            break;
        }
        has_color |= arg.to_str().unwrap_or("").starts_with("--color");
        args.push(arg.into());
    }

    if !has_color {
        let color = stderr_isatty();
        let setting = if color { "always" } else { "never" };
        args.push(format!("--color={}", setting).into());
    }

    args.push("--".into());
    args.push("--emit=llvm-ir".into());
    args.push("-o".into());
    args.push(outfile.into());
    args.extend(it);
    args
}

fn filter_err(ignore: fn(&str) -> bool) -> ! {
    let mut line = String::new();
    while let Ok(n) = io::stdin().read_line(&mut line) {
        if n == 0 {
            break;
        }
        if !ignore(&line) {
            let _ = write!(&mut io::stderr(), "{}", line);
        }
        line.clear();
    }
    process::exit(0);
}

fn ignore_cargo_err(line: &str) -> bool {
    if line.trim().is_empty() {
        return true;
    }

    let blacklist = [
        "ignoring specified output filename because multiple outputs were \
         requested",
        "ignoring specified output filename for 'link' output because multiple \
         outputs were requested",
        "ignoring --out-dir flag due to -o flag.",
        "due to multiple output types requested, the explicitly specified \
         output file name will be adapted for each output type",
    ];
    for s in &blacklist {
        if line.contains(s) {
            return true;
        }
    }

    false
}
