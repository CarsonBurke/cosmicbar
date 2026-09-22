//! What a process in the memory popup actually is.
//!
//! `/proc/<pid>/comm` names the executable, which for anything interpreted is
//! the interpreter: five rows of `python` say nothing about which job is eating
//! the memory. The command line names the script or module, the working
//! directory (or the virtualenv the interpreter belongs to) names the project,
//! and the process's cgroup names what launched it — a systemd service, an app
//! scope. Each is one small read, and they are only taken for the handful of
//! processes the popup lists.

use std::path::{Path, PathBuf};

/// Kernel limit on `comm`: TASK_COMM_LEN minus its NUL.
const COMM_MAX: usize = 15;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Origin {
    /// What the process runs: the script or module an interpreter was handed,
    /// otherwise the executable's own name.
    pub name: String,
    /// Where it came from, most specific first: the interpreter when `name` is
    /// a script, the project, the launching unit. Only what `name` does not
    /// already say.
    pub context: Vec<String>,
}

/// Identify `pid`. Everything past `comm` is best effort: a process that exits
/// mid-read, or one whose `cwd` is not ours to read, is still listed by name.
pub fn identify(pid: u32) -> Origin {
    let proc = PathBuf::from(format!("/proc/{pid}"));
    // Lossy: a multibyte name cut at 15 bytes is not UTF-8 any more.
    let comm = std::fs::read(proc.join("comm"))
        .map(|name| String::from_utf8_lossy(&name).trim_end().to_string())
        .unwrap_or_default();
    let cmdline = std::fs::read(proc.join("cmdline")).unwrap_or_default();
    let mut args: Vec<String> = cmdline
        .split(|byte| *byte == 0)
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    // The terminating NUL, and the padding a retitled process leaves behind;
    // an empty argument in the middle is still an argument.
    while args.last().is_some_and(String::is_empty) {
        args.pop();
    }
    // A directory removed under the process reads as `… (deleted)`.
    let cwd = std::fs::read_link(proc.join("cwd")).ok().map(|cwd| {
        match cwd.to_str().and_then(|cwd| cwd.strip_suffix(" (deleted)")) {
            Some(existed) => PathBuf::from(existed),
            None => cwd,
        }
    });
    let cgroup = std::fs::read_to_string(proc.join("cgroup")).unwrap_or_default();

    let mut origin = describe(&comm, &args, cwd.as_deref(), &cgroup, home().as_deref());
    if origin.name.is_empty() {
        origin.name = format!("[{pid}]");
    }
    origin
}

/// [`identify`] without the reads.
fn describe(
    comm: &str,
    args: &[String],
    cwd: Option<&Path>,
    cgroup: &str,
    home: Option<&Path>,
) -> Origin {
    let argv0 = args.first().map(|arg| basename(arg)).unwrap_or_default();
    // `comm` is cut at 15 bytes (`Isolated Web Co`); argv[0] usually is not.
    // Read lossily, a multibyte character cut in half ends it as U+FFFD.
    let stem = comm.trim_end_matches('\u{FFFD}');
    let truncated = |full: &str| comm.len() >= COMM_MAX && full.starts_with(stem);
    let own = if comm.is_empty() || truncated(argv0) {
        argv0
    } else {
        comm
    };

    let mut context = Vec::new();
    // A `#!` script execs with its own name as `comm` and the interpreter as
    // argv[0], so the interpreter is recognised by either.
    let name = match interpreter(argv0).or_else(|| interpreter(comm)) {
        Some(interpreter) => {
            context.push(interpreter.family.to_string());
            // A zombie or a kernel thread has no command line at all.
            match interpreter.target(args.get(1..).unwrap_or_default()) {
                // A process that retitled itself knows its name better than
                // its command line does; a `#!` script's own `comm` is only
                // its name cut short.
                Some(target) if interpreter.is(comm) || truncated(&target) => target,
                _ => own.to_string(),
            }
        }
        None => own.to_string(),
    };
    // The interpreter only adds something once the name is no longer it.
    if context.first().is_some_and(|interpreter| interpreter == family(&name)) {
        context.clear();
    }

    if let Some(project) = project(args.first().map(String::as_str), cwd, home) {
        context.push(project);
    }
    if let Some(unit) = unit(cgroup) {
        context.push(unit);
    }
    let mut seen = vec![name.to_lowercase()];
    context.retain(|entry| {
        let key = entry.to_lowercase();
        let new = !seen.contains(&key);
        seen.push(key);
        new
    });

    Origin { name, context }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `python3.12` → `python`: the version suffix is noise in a label.
fn family(name: &str) -> &str {
    name.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.')
}

/// How one interpreter's command line names the program it runs.
struct Interpreter {
    family: &'static str,
    /// Options followed by a separate value that is not the program.
    valued: &'static [&'static str],
    /// Options whose value *is* the program: `python -m pytest`, `java -jar`.
    program: &'static [&'static str],
    /// Options that run code from the command line itself: there is no program
    /// to name.
    inline: &'static [&'static str],
    /// Leading words that pick a subcommand rather than a program: `deno run`.
    subcommands: &'static [&'static str],
}

const INTERPRETERS: &[Interpreter] = &[
    Interpreter {
        family: "python",
        valued: &["-W", "-X", "-Q", "--check-hash-based-pycs"],
        program: &["-m"],
        inline: &["-c"],
        subcommands: &[],
    },
    Interpreter {
        family: "pypy",
        valued: &["-W", "-X"],
        program: &["-m"],
        inline: &["-c"],
        subcommands: &[],
    },
    Interpreter {
        family: "node",
        valued: &["-r", "--require", "--import", "--loader", "--conditions", "-C"],
        program: &[],
        inline: &["-e", "--eval", "-p", "--print"],
        subcommands: &[],
    },
    Interpreter {
        family: "bun",
        valued: &["-r", "--preload", "--cwd"],
        program: &[],
        inline: &["-e", "--eval", "-p", "--print"],
        subcommands: &["run", "test", "x"],
    },
    Interpreter {
        family: "deno",
        valued: &["--config", "-c", "--import-map"],
        program: &[],
        inline: &["eval"],
        subcommands: &["run", "test", "serve", "task"],
    },
    Interpreter {
        family: "ruby",
        valued: &["-r", "-I", "-C"],
        program: &[],
        inline: &["-e"],
        subcommands: &[],
    },
    Interpreter {
        family: "perl",
        valued: &["-I", "-M"],
        program: &[],
        inline: &["-e", "-E"],
        subcommands: &[],
    },
    Interpreter {
        family: "java",
        valued: &["-cp", "-classpath", "--class-path", "-p", "--module-path"],
        program: &["-jar", "-m", "--module"],
        inline: &[],
        subcommands: &[],
    },
    Interpreter {
        family: "julia",
        valued: &["--project", "-t", "--threads", "-L", "--load"],
        program: &[],
        inline: &["-e", "--eval", "-E", "--print"],
        subcommands: &[],
    },
    Interpreter {
        family: "Rscript",
        valued: &[],
        program: &[],
        inline: &["-e"],
        subcommands: &[],
    },
    Interpreter {
        family: "lua",
        valued: &["-l"],
        program: &[],
        inline: &["-e"],
        subcommands: &[],
    },
    Interpreter {
        family: "bash",
        valued: &["-o", "-O", "--rcfile", "--init-file"],
        program: &[],
        inline: &["-c"],
        subcommands: &[],
    },
    Interpreter {
        family: "sh",
        valued: &["-o"],
        program: &[],
        inline: &["-c"],
        subcommands: &[],
    },
    Interpreter {
        family: "zsh",
        valued: &["-o"],
        program: &[],
        inline: &["-c"],
        subcommands: &[],
    },
    Interpreter {
        family: "fish",
        valued: &["-C", "--init-command"],
        program: &[],
        inline: &["-c", "--command"],
        subcommands: &[],
    },
];

fn interpreter(name: &str) -> Option<&'static Interpreter> {
    let family = family(name);
    INTERPRETERS
        .iter()
        .find(|interpreter| interpreter.family == family)
}

impl Interpreter {
    fn is(&self, name: &str) -> bool {
        family(name) == self.family
    }

    /// The program in the interpreter's arguments: a script's file name, a
    /// module's dotted name, a jar. `None` for code passed inline, or for an
    /// interactive interpreter.
    fn target(&self, args: &[String]) -> Option<String> {
        let mut args = args.iter();
        let mut subcommand = true;
        while let Some(arg) = args.next() {
            let arg = arg.as_str();
            if self.inline.contains(&arg) {
                return None;
            }
            if self.program.contains(&arg) {
                return args.next().map(|program| program_name(program));
            }
            if self.valued.contains(&arg) {
                args.next();
                continue;
            }
            if arg == "--" {
                return args.next().map(|program| program_name(program));
            }
            // `-u`, `--inspect=9229`, `-Wignore`: a flag, or one carrying its
            // value with it.
            if arg.starts_with('-') {
                continue;
            }
            if std::mem::take(&mut subcommand) && self.subcommands.contains(&arg) {
                continue;
            }
            return Some(program_name(arg));
        }
        None
    }
}

/// A path's file name; a dotted module or class name as it is. A package run
/// by path (`…/compile_worker/__main__.py`) is named by its directory, the way
/// `python -m` would have named it.
fn program_name(program: &str) -> String {
    let path = Path::new(program);
    if path.file_name().is_some_and(|name| name == "__main__.py")
        && let Some(package) = path.parent().and_then(Path::file_name)
    {
        return package.to_string_lossy().into_owned();
    }
    basename(program).to_string()
}

/// The project a process belongs to. An interpreter inside a virtualenv was
/// made for the project the virtualenv sits in, wherever it was started from —
/// a queued job runs from a scratch directory, but its `.venv` still says whose
/// job it is. Failing that, the directory it was started in, unless that is
/// the home directory, the root, or a sandbox's pseudo-filesystem, which say
/// nothing.
fn project(argv0: Option<&str>, cwd: Option<&Path>, home: Option<&Path>) -> Option<String> {
    if let Some(argv0) = argv0.filter(|argv0| argv0.contains('/')) {
        let path = match (Path::new(argv0).is_absolute(), cwd) {
            (true, _) => PathBuf::from(argv0),
            (false, Some(cwd)) => cwd.join(argv0),
            (false, None) => PathBuf::from(argv0),
        };
        let venv = path.ancestors().find(|ancestor| {
            ancestor
                .file_name()
                .is_some_and(|name| name == ".venv" || name == "venv")
        });
        if let Some(name) = venv
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
        {
            return Some(name.to_string());
        }
    }
    let cwd = cwd?;
    // A Chromium renderer chroots into `/proc/<pid>/fdinfo`.
    let virtual_fs = ["/proc", "/sys", "/dev"]
        .iter()
        .any(|root| cwd.starts_with(root));
    if virtual_fs || cwd == Path::new("/") || Some(cwd) == home {
        return None;
    }
    cwd.file_name()?.to_str().map(str::to_string)
}

/// The systemd unit a process runs under, as a person would name it:
/// `mlqd.service` → `mlqd`, `app-orca-18094.scope` → `orca`,
/// `app-flatpak-net.nokyan.Resources-2656401153.scope` → `Resources`. `None`
/// for the units every process of a login has in common.
///
/// Desktop launchers follow systemd's naming convention for applications,
/// `app[-<launcher>]-<ApplicationID>[-<RANDOM>].scope` (`@<RANDOM>` for a
/// service), with any `-` inside the application id escaped as `\x2d` — which
/// is what makes the unescaped dashes safe to split on.
fn unit(cgroup: &str) -> Option<String> {
    // cgroup v2 has one line, `0::/path`.
    let path = cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?
        .trim();
    let leaf = path.rsplit('/').next()?;
    let (stem, kind) = leaf.rsplit_once('.')?;
    if kind != "service" && kind != "scope" {
        return None;
    }
    // The session itself, the user manager, the root scope, and the names
    // `systemd-run` makes up for an unnamed transient unit
    // (`run-p3912511-i3953478`, `run-u42`), which say nothing.
    if stem.starts_with("session-")
        || stem.starts_with("user@")
        || stem == "init"
        || generated_run_unit(stem)
    {
        return None;
    }
    // A container's scope is named for its hash.
    if stem.starts_with("libpod-") {
        return Some("podman".to_string());
    }
    if stem.starts_with("docker-") {
        return Some("docker".to_string());
    }
    // A bus-activated service, `dbus-:1.2-org.freedesktop.portal.Desktop@0`:
    // the bus's connection name, then the service's.
    if let Some(service) = stem
        .strip_prefix("dbus-:")
        .and_then(|rest| rest.split_once('-'))
        .map(|(_, service)| service)
    {
        return application(service.split_once('@').map_or(service, |(name, _)| name));
    }
    let Some(app) = stem.strip_prefix("app-") else {
        // A plain service is already named the way its author meant it.
        return Some(unescape(stem.split_once('@').map_or(stem, |(name, _)| name)));
    };
    // A template's instance (`Daemon@autostart`) is how it was started, not
    // what it is.
    let app = app.split_once('@').map_or(app, |(name, _)| name);
    let mut parts: Vec<&str> = app.split('-').collect();
    // The random suffix a launcher gives a scope: a pid, a hex word.
    if parts.len() > 1
        && parts.last().is_some_and(|id| {
            id.chars().all(|c| c.is_ascii_hexdigit()) && id.chars().any(|c| c.is_ascii_digit())
        })
    {
        parts.pop();
    }
    let (launcher, id) = match parts.as_slice() {
        [id] => (None, *id),
        [launcher, id] => (Some(*launcher), *id),
        // Not following the convention: keep it whole rather than guess.
        _ => (None, app),
    };
    // A launcher that ran a wrapper (`niri` spawning `env FOO=1 app`) says
    // more than the wrapper does.
    if let Some(launcher) = launcher
        && WRAPPERS.contains(&unescape(id).as_str())
    {
        return Some(unescape(launcher));
    }
    application(id)
}

/// A reverse-DNS application id is named by what follows its domain.
fn application(id: &str) -> Option<String> {
    let id = unescape(id);
    let name = match id.split('.').collect::<Vec<_>>().as_slice() {
        [_, _, rest @ ..] if !rest.is_empty() => rest.join("."),
        _ => id,
    };
    (!name.is_empty()).then_some(name)
}

/// `run-u<n>` or `run-p<pid>-i<n>`: systemd-run's name for a unit it was not
/// given one for.
fn generated_run_unit(stem: &str) -> bool {
    let Some(rest) = stem.strip_prefix("run-") else {
        return false;
    };
    let numbered = |part: &str, prefix: char| {
        part.strip_prefix(prefix)
            .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
    };
    match rest.split_once('-') {
        Some((pid, invocation)) => numbered(pid, 'p') && numbered(invocation, 'i'),
        None => numbered(rest, 'u'),
    }
}

/// Programs that only exist to start another one.
const WRAPPERS: &[&str] = &["env", "sh", "bash", "exec", "systemd-run"];

/// systemd's unit-name escaping: `\x2d` → `-`.
fn unescape(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(at) = rest.find("\\x") {
        out.push_str(&rest[..at]);
        let code = rest
            .get(at + 2..at + 4)
            .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            .filter(u8::is_ascii);
        match code {
            Some(byte) => {
                out.push(byte as char);
                rest = &rest[at + 4..];
            }
            None => {
                out.push_str("\\x");
                rest = &rest[at + 2..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// This bar's home directory, which as a working directory identifies nothing.
fn home() -> Option<PathBuf> {
    static HOME: std::sync::LazyLock<Option<PathBuf>> = std::sync::LazyLock::new(|| {
        std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
    });
    HOME.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    fn origin(comm: &str, line: &str, cwd: &str, cgroup: &str) -> Origin {
        describe(
            comm,
            &args(line),
            Some(Path::new(cwd)),
            cgroup,
            Some(Path::new("/home/me")),
        )
    }

    #[test]
    fn a_venv_script_names_the_script_its_project_and_its_launcher() {
        let origin = origin(
            "python",
            "/home/me/repos/kraggiculture/.venv/bin/python /var/tmp/scratch/update_harness",
            "/var/tmp/scratch/frozen/b561",
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/mlqd.service",
        );
        assert_eq!(origin.name, "update_harness");
        assert_eq!(origin.context, ["python", "kraggiculture", "mlqd"]);
    }

    #[test]
    fn a_relative_venv_resolves_against_the_working_directory() {
        let origin = origin(
            "python",
            ".venv/bin/python -u /tmp/scratchpad/time_prep.py",
            "/home/me/repos/parameter-golf",
            "0::/user.slice/user@1000.service/app.slice/app-orca-18094.scope",
        );
        assert_eq!(origin.name, "time_prep.py");
        assert_eq!(origin.context, ["python", "parameter-golf", "orca"]);
    }

    #[test]
    fn a_module_run_is_named_by_its_module() {
        let origin = origin(
            "python3.12",
            "/usr/bin/python3.12 -X dev -m pytest -q tests/test_ppo.py",
            "/home/me/repos/kraggiculture",
            "",
        );
        assert_eq!(origin.name, "pytest");
        assert_eq!(origin.context, ["python", "kraggiculture"]);
    }

    #[test]
    fn a_shebang_script_keeps_its_own_name_and_names_its_interpreter() {
        let origin = origin(
            "pytest",
            "/home/me/repos/app/.venv/bin/python /home/me/repos/app/.venv/bin/pytest -q",
            "/home/me/repos/app",
            "",
        );
        assert_eq!(origin.name, "pytest");
        assert_eq!(origin.context, ["python", "app"]);
    }

    #[test]
    fn inline_code_leaves_the_interpreter_as_the_name() {
        let origin = origin("python", "python -c print(1)", "/home/me", "");
        assert_eq!(origin.name, "python");
        assert!(origin.context.is_empty());
    }

    #[test]
    fn a_retitled_interpreter_keeps_its_title() {
        let origin = origin("ray::Worker", "python worker.py", "/home/me", "");
        assert_eq!(origin.name, "ray::Worker");
        assert_eq!(origin.context, ["python"]);
    }

    #[test]
    fn a_long_shebang_script_is_named_in_full() {
        let origin = origin(
            "train_policy_pp",
            "/home/me/repos/k/.venv/bin/python /home/me/repos/k/train_policy_ppo.py",
            "/home/me/repos/k",
            "",
        );
        assert_eq!(origin.name, "train_policy_ppo.py");
        assert_eq!(origin.context, ["python", "k"]);
    }

    #[test]
    fn a_comm_cut_inside_a_character_is_completed() {
        let origin = describe("Isolated Web C\u{FFFD}", &["/usr/lib/Isolated Web Cö".into()], None, "", None);
        assert_eq!(origin.name, "Isolated Web Cö");
    }

    #[test]
    fn an_unreadable_comm_falls_back_to_argv0() {
        let origin = describe("", &["/usr/lib/firefox/Isolated Web Content".into()], None, "", None);
        assert_eq!(origin.name, "Isolated Web Content");
    }

    #[test]
    fn a_versioned_interactive_interpreter_is_not_repeated() {
        let origin = origin("python3", "python3", "/home/me", "");
        assert_eq!(origin.name, "python3");
        assert!(origin.context.is_empty());
    }

    #[test]
    fn subcommands_and_valued_options_are_skipped() {
        let bash = interpreter("bash").unwrap();
        assert_eq!(bash.target(&args("--rcfile /home/me/.bashrc -i")), None);
        let deno = interpreter("deno").unwrap();
        assert_eq!(
            deno.target(&args("run --config deno.json main.ts")),
            Some("main.ts".into())
        );
        let java = interpreter("java").unwrap();
        assert_eq!(
            java.target(&args("-Xmx4g -cp lib/* -jar /opt/app/server.jar")),
            Some("server.jar".into())
        );
        let node = interpreter("node").unwrap();
        assert_eq!(
            node.target(&args("--inspect=9229 -r ts-node/register src/index.ts")),
            Some("index.ts".into())
        );
    }

    #[test]
    fn a_truncated_comm_is_completed_from_argv0() {
        let origin = describe(
            "Isolated Web Co",
            &["/usr/lib/firefox/Isolated Web Content".to_string()],
            None,
            "",
            None,
        );
        assert_eq!(origin.name, "Isolated Web Content");
    }

    #[test]
    fn a_package_run_by_path_is_named_by_its_package() {
        let origin = origin(
            "python",
            "/home/me/repos/k/.venv/bin/python /home/me/repos/k/.venv/lib/torch/compile_worker/__main__.py --pickler=x",
            "/tmp/frozen",
            "",
        );
        assert_eq!(origin.name, "compile_worker");
        assert_eq!(origin.context, ["python", "k"]);
    }

    #[test]
    fn a_sandboxed_working_directory_is_not_a_project() {
        let origin = origin("helium", "/opt/helium/helium --type=renderer", "/proc/7332/fdinfo", "");
        assert!(origin.context.is_empty());
    }

    #[test]
    fn an_interpreter_without_a_command_line_is_still_named() {
        let origin = describe("python", &[], None, "", None);
        assert_eq!(origin.name, "python");
        assert!(origin.context.is_empty());
    }

    #[test]
    fn a_native_program_in_home_has_only_its_launcher() {
        let origin = origin(
            "resources",
            "resources",
            "/home/me",
            "0::/user.slice/user@1000.service/app.slice/app-flatpak-net.nokyan.Resources-2656401153.scope",
        );
        assert_eq!(origin.name, "resources");
        // `Resources` is the name again, in another case.
        assert!(origin.context.is_empty());
    }

    #[test]
    fn units_read_as_names() {
        let unit = |leaf: &str| unit(&format!("0::/user.slice/app.slice/{leaf}"));
        assert_eq!(unit("mlqd.service").as_deref(), Some("mlqd"));
        assert_eq!(unit("app-orca-18094.scope").as_deref(), Some("orca"));
        assert_eq!(unit("app-niri-env-34abeef6.scope").as_deref(), Some("niri"));
        assert_eq!(
            unit(r"app-niri-niri\x2dopen\x2ddefault\x2dbrowser-7181.scope").as_deref(),
            Some("niri-open-default-browser")
        );
        assert_eq!(
            unit("app-dev.cagents.Cagents.Daemon@autostart.service").as_deref(),
            Some("Cagents.Daemon")
        );
        assert_eq!(
            unit("app-org.chromium.Chromium-7250.scope").as_deref(),
            Some("Chromium")
        );
        assert_eq!(unit("dbus-broker.service").as_deref(), Some("dbus-broker"));
        assert_eq!(
            unit("dbus-:1.2-org.freedesktop.portal.Desktop@0.service").as_deref(),
            Some("portal.Desktop")
        );
        assert_eq!(unit("libpod-4f1c0d9e2b7a.scope").as_deref(), Some("podman"));
        assert_eq!(unit("session-2.scope"), None);
        assert_eq!(unit("run-p3912511-i3953478.scope"), None);
        assert_eq!(unit("run-u42.service"), None);
        assert_eq!(unit("run-backup.service").as_deref(), Some("run-backup"));
        assert_eq!(unit("user@1000.service"), None);
        assert_eq!(unit("app.slice"), None);
    }
}
