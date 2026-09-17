//! Compare cosmicbar with waybar on the live desktop, including reaped-child CPU.
//!
//! cargo run --release --example measure-bar -- target/release/cosmicbar mine 150
//!
//! Replaces a bar launched from the same path, waits for startup to settle, and
//! samples both bars over the same window. Leaves cosmicbar running afterward.

use std::env;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

const SETTLE: Duration = Duration::from_secs(12);
const USAGE: &str = "Usage: cargo run --release --example measure-bar -- BINARY LABEL SECONDS\n\
    Replaces BINARY's running bar and compares its CPU/RSS with waybar.\n\
    Includes reaped-child CPU and counts redraw messages in /tmp/measure-bar-LABEL.log.\n\
    Leaves the measured bar running afterward.";

fn cpu_seconds(pid: u32, hz: f64) -> Result<f64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))
        .with_context(|| format!("reading CPU time for process {pid}"))?;
    let (_, fields) = stat.rsplit_once(") ").context("invalid process stat")?;
    // After the parenthesized command: utime, stime, cutime, cstime (fields 14–17).
    let mut ticks = 0_i64;
    let mut count = 0;
    for field in fields.split_whitespace().skip(11).take(4) {
        ticks += field.parse::<i64>()?;
        count += 1;
    }
    ensure!(count == 4, "process stat is missing CPU fields");
    Ok(ticks as f64 / hz)
}

fn rss_kb(pid: u32) -> Result<u64> {
    let status = fs::read_to_string(format!("/proc/{pid}/status"))
        .with_context(|| format!("reading RSS for process {pid}"))?;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("VmRSS:") {
            return Ok(value
                .split_whitespace()
                .next()
                .context("empty VmRSS")?
                .parse()?);
        }
    }
    Ok(0)
}

fn process_ids() -> Result<Vec<u32>> {
    let mut pids = Vec::new();
    for entry in fs::read_dir("/proc")? {
        if let Ok(pid) = entry?.file_name().to_string_lossy().parse() {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    Ok(pids)
}

fn matches_binary(pid: u32, binary: &Path) -> bool {
    // Like an anchored full-command match: only this path, without arguments.
    // Compare bytes rather than treating special characters in paths as regexes.
    fs::read(format!("/proc/{pid}/cmdline"))
        .is_ok_and(|cmdline| cmdline.strip_suffix(&[0]) == Some(binary.as_os_str().as_bytes()))
}

fn waybar_pid() -> Result<Option<u32>> {
    Ok(process_ids()?.into_iter().find(|pid| {
        fs::read_to_string(format!("/proc/{pid}/comm"))
            .is_ok_and(|name| name.trim_end_matches('\n') == "waybar")
    }))
}

fn redraws(log: &Path) -> Result<u64> {
    let mut count = 0;
    for line in BufReader::new(File::open(log)?).lines() {
        if line?.contains(" update ") {
            count += 1;
        }
    }
    Ok(count)
}

fn main() -> Result<()> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    if args.len() == 1 && (args[0] == "--help" || args[0] == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    ensure!(args.len() == 3, "{USAGE}");
    let binary = std::path::absolute(&args[0])?;
    let label = args[1].to_str().context("LABEL must be UTF-8")?;
    let seconds: f64 = args[2]
        .to_str()
        .context("SECONDS must be UTF-8")?
        .parse()
        .context("SECONDS must be a number")?;
    ensure!(seconds > 0.0, "SECONDS must be positive");
    let window = Duration::try_from_secs_f64(seconds).context("invalid sampling duration")?;
    // SAFETY: sysconf takes only the constant selector and does not retain pointers.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    ensure!(hz > 0, "could not determine the process clock tick rate");
    let hz = hz as f64;
    let log_path = PathBuf::from(format!("/tmp/measure-bar-{label}.log"));

    for pid in process_ids()? {
        if matches_binary(pid, &binary) {
            // SAFETY: pid came from /proc, and SIGTERM is a valid signal.
            if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error).context("stopping the previous bar");
                }
            }
        }
    }
    thread::sleep(Duration::from_secs(1));
    let runtime = env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| "/tmp".into());
    let display = env::var_os("WAYLAND_DISPLAY").unwrap_or_else(|| "wayland-1".into());
    let mut socket_name = OsStr::new("cosmicbar-").to_os_string();
    socket_name.push(display);
    socket_name.push(".sock");
    let socket = PathBuf::from(runtime).join(socket_name);
    match fs::remove_file(&socket) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("removing the stale bar socket"),
    }

    let log = File::create(&log_path)?;
    let mut command = Command::new(&binary);
    command
        .stdout(log.try_clone()?)
        .stderr(log)
        .env("RUST_LOG", "warn,cosmicbar=debug");
    // SAFETY: the child hook calls only async-signal-safe setsid and constructs
    // an OS error on failure, without allocation or touching inherited locks.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {}", binary.display()))?;
    thread::sleep(SETTLE);
    if child.try_wait()?.is_some() {
        bail!("{label}: bar did not start; see {}", log_path.display());
    }
    let bar = child.id();
    let way = waybar_pid()?;
    let before = redraws(&log_path)?;
    let bar_start = cpu_seconds(bar, hz)?;
    let way_start = way.map(|pid| cpu_seconds(pid, hz)).transpose()?;
    let start = Instant::now();
    thread::sleep(window);
    let elapsed = start.elapsed().as_secs_f64();
    let messages = i128::from(redraws(&log_path)?) - i128::from(before);

    println!("== {label} over {elapsed:.0}s");
    println!(
        "cosmicbar cpu   {:.3}%",
        100.0 * (cpu_seconds(bar, hz)? - bar_start) / elapsed
    );
    println!("cosmicbar rss   {} kB", rss_kb(bar)?);
    println!(
        "bar redraws     {messages} ({:.0}/min)",
        messages as f64 / elapsed * 60.0
    );
    if let Some((way, way_start)) = way.zip(way_start) {
        println!(
            "waybar cpu      {:.3}%",
            100.0 * (cpu_seconds(way, hz)? - way_start) / elapsed
        );
        println!("waybar rss      {} kB", rss_kb(way)?);
    } else {
        println!("waybar          not running; start it to compare");
    }
    Ok(())
}
