//! `fake-cli` — a tiny shim the supervisor tests point adapters at
//! (`plan/11-testing.md` §Worker adapter testing). It replays a fixture to
//! stdout or misbehaves on purpose; it never talks to any model.
//!
//! Flags (all optional, order-free):
//! - `--version`              print `fake-cli <version>` and exit 0
//! - `--fixture <path>`       print the file's lines to stdout
//! - `--lines <n> --bytes <b>` flood stdout with `n` lines of `b` bytes each (incl. newline)
//! - `--stderr <text>`        print `text` to stderr
//! - `--echo-env`             print `KEY=VALUE` lines of the environment, sorted
//! - `--print-cwd`            print the current directory
//! - `--print-stdin`          copy stdin to stdout
//! - `--spawn-child`          spawn a hanging grandchild, print `child_pid=<pid>`, then hang
//! - `--ignore-sigterm`       install a SIGTERM handler so only SIGKILL stops us
//! - `--sleep-ms <ms>`        sleep before exiting
//! - `--hang`                 sleep forever
//! - `--abort`                abort (SIGABRT)
//! - `--exit <code>`          exit code (default 0)

use std::io::{BufRead as _, Read as _, Write as _};
use std::time::Duration;

struct Opts {
    version: bool,
    fixture: Option<String>,
    lines: usize,
    bytes: usize,
    stderr: Option<String>,
    echo_env: bool,
    print_cwd: bool,
    print_stdin: bool,
    spawn_child: bool,
    ignore_sigterm: bool,
    sleep_ms: u64,
    hang: bool,
    abort: bool,
    exit: i32,
}

fn parse() -> Opts {
    let mut opts = Opts {
        version: false,
        fixture: None,
        lines: 0,
        bytes: 0,
        stderr: None,
        echo_env: false,
        print_cwd: false,
        print_stdin: false,
        spawn_child: false,
        ignore_sigterm: false,
        sleep_ms: 0,
        hang: false,
        abort: false,
        exit: 0,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().unwrap_or_default();
        match arg.as_str() {
            "--version" => opts.version = true,
            "--fixture" => opts.fixture = Some(value()),
            "--lines" => opts.lines = value().parse().unwrap_or(0),
            "--bytes" => opts.bytes = value().parse().unwrap_or(0),
            "--stderr" => opts.stderr = Some(value()),
            "--echo-env" => opts.echo_env = true,
            "--print-cwd" => opts.print_cwd = true,
            "--print-stdin" => opts.print_stdin = true,
            "--spawn-child" => opts.spawn_child = true,
            "--ignore-sigterm" => opts.ignore_sigterm = true,
            "--sleep-ms" => opts.sleep_ms = value().parse().unwrap_or(0),
            "--hang" => opts.hang = true,
            "--abort" => opts.abort = true,
            "--exit" => opts.exit = value().parse().unwrap_or(0),
            _ => {}
        }
    }
    opts
}

fn main() {
    let opts = parse();
    if opts.version {
        println!("fake-cli {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    // Registering a tokio signal handler replaces SIGTERM's default action for
    // the lifetime of the process (safe code only; `unsafe_code` is forbidden).
    let runtime = if opts.ignore_sigterm {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let guard = rt.enter();
        let signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("signal handler");
        drop(guard);
        Some((rt, signal))
    } else {
        None
    };
    run(&opts);
    drop(runtime);
    std::process::exit(opts.exit);
}

fn run(opts: &Opts) {
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    if opts.print_cwd {
        let cwd = std::env::current_dir().unwrap_or_default();
        let _ = writeln!(out, "cwd={}", cwd.display());
    }
    if opts.echo_env {
        let mut vars: Vec<(String, String)> = std::env::vars().collect();
        vars.sort();
        for (k, v) in vars {
            let _ = writeln!(out, "{k}={v}");
        }
    }
    if opts.print_stdin {
        let mut input = String::new();
        let _ = std::io::stdin().lock().read_to_string(&mut input);
        for line in input.lines() {
            let _ = writeln!(out, "stdin:{line}");
        }
    }
    if let Some(path) = &opts.fixture {
        if let Ok(file) = std::fs::File::open(path) {
            for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
                let _ = writeln!(out, "{line}");
            }
        } else {
            eprintln!("fake-cli: cannot open fixture {path}");
        }
    }
    if opts.lines > 0 {
        let width = opts.bytes.saturating_sub(1).max(1);
        let payload = "x".repeat(width);
        for _ in 0..opts.lines {
            let _ = writeln!(out, "{payload}");
        }
    }
    let _ = out.flush();
    if let Some(text) = &opts.stderr {
        eprintln!("{text}");
    }
    if opts.spawn_child {
        let exe = std::env::current_exe().expect("current exe");
        let child = std::process::Command::new(exe)
            .arg("--hang")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn grandchild");
        println!("child_pid={}", child.id());
        let _ = std::io::stdout().flush();
        // Keep the grandchild alive (and in our process group) until we die.
        std::mem::forget(child);
        hang();
    }
    if opts.abort {
        std::process::abort();
    }
    if opts.sleep_ms > 0 {
        std::thread::sleep(Duration::from_millis(opts.sleep_ms));
    }
    if opts.hang {
        hang();
    }
}

fn hang() -> ! {
    loop {
        std::thread::sleep(Duration::from_hours(1));
    }
}
