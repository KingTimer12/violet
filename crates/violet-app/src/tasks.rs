//! Running backup and restore programs, such as `pg_dump`, for the tasks
//! drivers describe.
//!
//! A driver only names a program and its arguments; this module decides
//! which file runs. It looks in the folder the user picked, then the
//! driver's usual locations, then `PATH`, starts the program directly (no
//! shell), passes the password in the variable the command names and
//! reports the program's output line by line.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::watch;
use violet_core::{DbError, ErrorKind, Result, Secret, Task, ToolCommand};

/// A program a task runs, as found on this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    pub name: String,
    pub path: Option<PathBuf>,
    /// The first line it prints for the task's version option.
    pub version: Option<String>,
}

/// How a finished task went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskSummary {
    pub elapsed: Duration,
    /// The size of what a backup wrote, in bytes.
    pub bytes: Option<u64>,
}

/// The file `name` runs as on this platform.
fn file_name(name: &str) -> String {
    if cfg!(windows) && !name.to_ascii_lowercase().ends_with(".exe") {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

/// The folders to look in, in order: the user's, the programs Violet
/// downloaded with the provider, the task's usual locations (a `*`
/// expanded, highest name first) and `PATH`.
#[must_use]
pub fn search_folders(
    task: &Task,
    folder: Option<&Path>,
    downloaded: Option<&Path>,
) -> Vec<PathBuf> {
    let mut folders: Vec<PathBuf> = folder
        .into_iter()
        .chain(downloaded)
        .map(Path::to_path_buf)
        .collect();
    for location in &task.locations {
        folders.extend(expand(location).into_iter().filter(|f| trusted_folder(f)));
    }
    if let Some(path) = std::env::var_os("PATH") {
        // Relative entries (`.`, empty) would run whatever sits in the
        // current folder.
        folders
            .extend(std::env::split_paths(&path).filter(|f| f.is_absolute() && trusted_folder(f)));
    }
    let mut seen = std::collections::HashSet::new();
    folders.retain(|folder| seen.insert(folder.clone()));
    folders
}

/// Folders anyone can drop a program into never count: temporary and
/// shared folders, and folders (or their parents) writable by everyone.
/// The folder the user picks is taken as they chose it.
fn trusted_folder(folder: &Path) -> bool {
    const SHARED: &[&str] = &[
        "/tmp",
        "/var/tmp",
        "/private/tmp",
        "/private/var/tmp",
        "/dev/shm",
        "/Users/Shared",
    ];
    if SHARED.iter().any(|shared| folder.starts_with(shared)) {
        return false;
    }
    if let Some(temp) = std::env::var_os("TMPDIR").or_else(|| std::env::var_os("TEMP"))
        && folder.starts_with(PathBuf::from(temp))
    {
        return false;
    }
    !folder.ancestors().any(world_writable)
}

#[cfg(unix)]
fn world_writable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|meta| {
        let mode = meta.permissions().mode();
        // The sticky bit (as on /tmp) still lets anyone add files.
        mode & 0o002 != 0
    })
}

#[cfg(not(unix))]
fn world_writable(_path: &Path) -> bool {
    false
}

/// The folders a location names. A `*` matches part of one folder name;
/// matches come highest version first.
fn expand(location: &str) -> Vec<PathBuf> {
    let Some(star) = location.find('*') else {
        return vec![PathBuf::from(location)];
    };
    let (before, after) = (&location[..star], &location[star + 1..]);
    let split = before.rfind(['/', '\\']).map_or(0, |i| i + 1);
    let (parent, prefix) = (&before[..split], &before[split..]);
    let end = after.find(['/', '\\']).unwrap_or(after.len());
    let (suffix, rest) = (&after[..end], &after[end..]);
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| {
            name.len() >= prefix.len() + suffix.len()
                && name.starts_with(prefix)
                && name.ends_with(suffix)
        })
        .collect();
    names.sort_by(|a, b| version_order(b, a));
    names
        .into_iter()
        .map(|name| PathBuf::from(format!("{parent}{name}{rest}")))
        .collect()
}

/// Compares names with their numbers as numbers, so `17` comes after `9`.
fn version_order(a: &str, b: &str) -> std::cmp::Ordering {
    let key = |name: &str| -> Vec<(u64, String)> {
        let mut parts = Vec::new();
        let mut digits = String::new();
        let mut text = String::new();
        for c in name.chars() {
            if c.is_ascii_digit() {
                if !text.is_empty() {
                    parts.push((0, std::mem::take(&mut text)));
                }
                digits.push(c);
            } else {
                if !digits.is_empty() {
                    parts.push((digits.parse().unwrap_or(u64::MAX), String::new()));
                    digits.clear();
                }
                text.push(c);
            }
        }
        if !digits.is_empty() {
            parts.push((digits.parse().unwrap_or(u64::MAX), String::new()));
        }
        if !text.is_empty() {
            parts.push((0, text));
        }
        parts
    };
    key(a).cmp(&key(b))
}

/// Where `name` is, in the first folder that has it.
#[must_use]
pub fn find(name: &str, folders: &[PathBuf]) -> Option<PathBuf> {
    let file = file_name(name);
    folders
        .iter()
        .map(|folder| folder.join(&file))
        .find(|path| is_executable(path))
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        // A program anyone could have rewritten is not run.
        meta.is_file() && mode & 0o111 != 0 && mode & 0o002 == 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// Finds each of the task's programs and asks it its version.
pub async fn locate(task: &Task, folder: Option<&Path>, downloaded: Option<&Path>) -> Vec<Program> {
    let folders = search_folders(task, folder, downloaded);
    let mut programs = Vec::with_capacity(task.programs.len());
    for name in &task.programs {
        let path = find(name, &folders);
        let version = match (&path, &task.version_arg) {
            (Some(path), Some(arg)) => version(path, arg).await,
            _ => None,
        };
        programs.push(Program {
            name: name.clone(),
            path,
            version,
        });
    }
    programs
}

async fn version(path: &Path, arg: &str) -> Option<String> {
    let mut command = Command::new(path);
    command
        .arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    no_window(&mut command);
    let output = tokio::time::timeout(Duration::from_secs(5), command.output())
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .next()
        .map(|line| line.trim().chars().take(120).collect())
}

/// Keeps console windows from flashing up on Windows.
fn no_window(command: &mut Command) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

/// What [`run`] needs, checked against its task already.
pub struct Run {
    pub program: PathBuf,
    pub command: ToolCommand,
    pub password: Option<Secret>,
    pub path: PathBuf,
}

/// Runs a command to the end, or until `cancel` turns true. Each line the
/// program prints goes to `line`.
pub async fn run(
    run: Run,
    mut cancel: watch::Receiver<bool>,
    line: impl Fn(String) + Send + Sync + 'static,
) -> Result<TaskSummary> {
    let started = Instant::now();
    let Run {
        program,
        command,
        password,
        path,
    } = run;
    let mut process = Command::new(&program);
    process
        .args(&command.args)
        .envs(command.env.iter().map(|(k, v)| (k, v)))
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    no_window(&mut process);
    if let Some(name) = &command.password_env {
        match &password {
            Some(password) => process.env(name, password.expose()),
            // Never one left over from Violet's own environment.
            None => process.env_remove(name),
        };
    }
    let mut input = None;
    if command.stdin_from_path {
        let file = std::fs::File::open(&path).map_err(|error| io("open the input", &error))?;
        if command.stdin_lines.is_empty() {
            process.stdin(Stdio::from(file));
        } else {
            process.stdin(Stdio::piped());
            input = Some(file);
        }
    } else {
        process.stdin(Stdio::null());
    }
    let mut child = process
        .spawn()
        .map_err(|error| io(&format!("start {}", program.display()), &error))?;

    let line = std::sync::Arc::new(line);
    let mut readers = Vec::new();
    if let (Some(file), Some(stdin)) = (input, child.stdin.take()) {
        let swaps = command.stdin_lines.clone();
        readers.push(tokio::spawn(async move {
            // The program may stop reading early, on an error of its own.
            let _ = feed(tokio::fs::File::from_std(file), stdin, &swaps).await;
        }));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(tokio::spawn(lines(stderr, std::sync::Arc::clone(&line))));
    }
    if let Some(mut stdout) = child.stdout.take() {
        if command.stdout_to_path {
            let target = path.clone();
            readers.push(tokio::spawn(async move {
                match tokio::fs::File::create(&target).await {
                    Ok(mut file) => {
                        let _ = tokio::io::copy(&mut stdout, &mut file).await;
                    }
                    Err(error) => log::error!("task output {}: {error}", target.display()),
                }
            }));
        } else {
            readers.push(tokio::spawn(lines(stdout, std::sync::Arc::clone(&line))));
        }
    }

    let status = tokio::select! {
        status = child.wait() => status.map_err(|error| io("wait for the program", &error))?,
        () = cancelled(&mut cancel) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            for reader in readers {
                let _ = reader.await;
            }
            return Err(DbError::new(ErrorKind::Cancelled, "Cancelled"));
        }
    };
    for reader in readers {
        let _ = reader.await;
    }
    if !status.success() {
        let code = status.code().map_or_else(
            || "was stopped".to_owned(),
            |code| format!("exited with {code}"),
        );
        return Err(DbError::new(
            ErrorKind::Other,
            format!(
                "{} {code}; see the log",
                program.file_name().unwrap_or_default().to_string_lossy()
            ),
        ));
    }
    let bytes = if command.stdin_from_path {
        None
    } else {
        let path = path.clone();
        tokio::task::spawn_blocking(move || size(&path))
            .await
            .ok()
            .flatten()
    };
    Ok(TaskSummary {
        elapsed: started.elapsed(),
        bytes,
    })
}

/// Copies `file` to `stdin` line by line, swapping whole lines, with or
/// without their `\r`.
async fn feed(
    file: tokio::fs::File,
    stdin: tokio::process::ChildStdin,
    swaps: &[(String, String)],
) -> std::io::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let mut reader = tokio::io::BufReader::new(file);
    let mut writer = tokio::io::BufWriter::new(stdin);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line).await? > 0 {
        let body = line.strip_suffix(b"\n").unwrap_or(&line);
        let body = body.strip_suffix(b"\r").unwrap_or(body);
        match swaps.iter().find(|(from, _)| from.as_bytes() == body) {
            Some((_, to)) => {
                writer.write_all(to.as_bytes()).await?;
                writer.write_all(&line[body.len()..]).await?;
            }
            None => writer.write_all(&line).await?,
        }
        line.clear();
    }
    writer.shutdown().await
}

async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow() {
            return;
        }
        if cancel.changed().await.is_err() {
            // Nobody can cancel any more: wait for the program.
            std::future::pending::<()>().await;
        }
    }
}

async fn lines(
    stream: impl AsyncRead + Unpin,
    line: std::sync::Arc<impl Fn(String) + Send + Sync + 'static>,
) {
    let mut reader = BufReader::new(stream);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let text = String::from_utf8_lossy(&buffer);
                let text = text.trim_end_matches(['\r', '\n']);
                line(text.chars().take(2000).collect());
            }
        }
    }
}

/// A file's size, or a folder's total.
fn size(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.is_file() {
        return Some(meta.len());
    }
    let mut total = 0;
    for entry in std::fs::read_dir(path).ok()?.flatten() {
        if let Ok(meta) = entry.metadata()
            && meta.is_file()
        {
            total += meta.len();
        }
    }
    Some(total)
}

fn io(what: &str, error: &std::io::Error) -> DbError {
    DbError::new(ErrorKind::Other, format!("Could not {what}: {error}"))
}

/// The first bytes of a file, or a folder's entry names, for a driver to
/// tell its format.
pub fn peek(path: &Path) -> std::io::Result<(Vec<u8>, Vec<String>, bool)> {
    use std::io::Read as _;
    let meta = std::fs::metadata(path)?;
    if meta.is_dir() {
        let mut names: Vec<String> = std::fs::read_dir(path)?
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .take(violet_core::INSPECT_ENTRIES * 4)
            .collect();
        names.sort();
        names.truncate(violet_core::INSPECT_ENTRIES);
        return Ok((Vec::new(), names, true));
    }
    let mut head = Vec::with_capacity(violet_core::INSPECT_HEAD);
    std::fs::File::open(path)?
        .take(violet_core::INSPECT_HEAD as u64)
        .read_to_end(&mut head)?;
    Ok((head, Vec::new(), false))
}

/// The command as a person would type it, to show before it runs. The
/// password is never in it: its variable shows as `•••`.
#[must_use]
pub fn display(program: &Path, command: &ToolCommand) -> String {
    let mut parts = Vec::new();
    for (name, value) in &command.env {
        parts.push(format!("{name}={}", shell_quote(value)));
    }
    if let Some(name) = &command.password_env {
        parts.push(format!("{name}=•••"));
    }
    parts.push(shell_quote(&program.to_string_lossy()));
    parts.extend(command.args.iter().map(|arg| shell_quote(arg)));
    let mut text = parts.join(" ");
    if command.stdout_to_path {
        text.push_str(" > output");
    }
    if command.stdin_from_path {
        text.push_str(" < input");
    }
    text
}

fn shell_quote(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_=./:,@+%".contains(c));
    if plain {
        text.to_owned()
    } else if !text.contains('\'') {
        format!("'{text}'")
    } else if !text.contains(['"', '$', '`', '\\', '!']) {
        // Reads better than escaping each single quote.
        format!("\"{text}\"")
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_sort_as_numbers() {
        let mut names = vec!["9.6", "17", "10", "postgresql@16", "postgresql@9"];
        names.sort_by(|a, b| version_order(b, a));
        assert_eq!(names, ["17", "10", "9.6", "postgresql@16", "postgresql@9"]);
    }

    #[test]
    fn stars_expand_to_existing_folders_newest_first() {
        let root = std::env::temp_dir().join(format!("violet-expand-{}", std::process::id()));
        for version in ["9", "16", "17", "other"] {
            std::fs::create_dir_all(root.join(format!("pg-{version}")).join("bin")).expect("mkdir");
        }
        let pattern = format!("{}/pg-*/bin", root.display());
        let found = expand(&pattern);
        let names: Vec<String> = found
            .iter()
            .map(|p| {
                p.parent()
                    .and_then(Path::file_name)
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, ["pg-other", "pg-17", "pg-16", "pg-9"]);
        assert!(expand(&format!("{}/nothing-*/bin", root.display())).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn shared_and_world_writable_folders_are_skipped() {
        assert!(!trusted_folder(Path::new("/tmp/pg/bin")));
        assert!(!trusted_folder(Path::new("/private/tmp/x")));
        assert!(trusted_folder(Path::new("/usr/bin")));
        let task = Task {
            kind: violet_core::TaskKind::Backup,
            programs: vec!["pg_dump".into()],
            locations: vec!["/tmp/*/bin".into()],
            version_arg: None,
            fields: Vec::new(),
            objects: false,
            targets: Vec::new(),
            connection: violet_core::Connection::default(),
            commands: Vec::new(),
        };
        let folders = search_folders(&task, Some(Path::new("/tmp/picked")), None);
        // The user's own pick stays; the shared location does not.
        assert_eq!(folders[0], Path::new("/tmp/picked"));
        assert!(!folders[1..].iter().any(|f| f.starts_with("/tmp")));
    }

    #[test]
    fn commands_show_without_the_password() {
        let command = ToolCommand {
            program: "pg_dump".into(),
            args: vec!["--dbname=host='x' port=5432".into(), "--format=c".into()],
            env: vec![("PGCONNECT_TIMEOUT".into(), "10".into())],
            password_env: Some("PGPASSWORD".into()),
            stdout_to_path: false,
            stdin_from_path: false,
            stdin_lines: Vec::new(),
        };
        assert_eq!(
            display(Path::new("/usr/bin/pg_dump"), &command),
            r#"PGCONNECT_TIMEOUT=10 PGPASSWORD=••• /usr/bin/pg_dump "--dbname=host='x' port=5432" --format=c"#
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runs_report_lines_failures_and_cancels() {
        use std::sync::{Arc, Mutex};
        let dir = std::env::temp_dir().join(format!("violet-run-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let out = dir.join("out.txt");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let command = |args: &[&str], password_env: Option<&str>| ToolCommand {
            program: "printf".into(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            env: Vec::new(),
            password_env: password_env.map(str::to_owned),
            stdout_to_path: false,
            stdin_from_path: false,
            stdin_lines: Vec::new(),
        };
        // Lines, and the size of what was written.
        let mut write = command(&["a\\nb\\n"], None);
        write.stdout_to_path = true;
        let summary = super::run(
            Run {
                program: PathBuf::from("/usr/bin/printf"),
                command: write,
                password: None,
                path: out.clone(),
            },
            watch::channel(false).1,
            move |line| sink.lock().expect("lock").push(line),
        )
        .await
        .expect("ran");
        assert_eq!(summary.bytes, Some(4));
        assert_eq!(std::fs::read_to_string(&out).expect("out"), "a\nb\n");

        let sink = Arc::clone(&seen);
        super::run(
            Run {
                program: PathBuf::from("/bin/sh"),
                command: command(&["-c", "echo \"$SECRET_PW\"; echo two"], Some("SECRET_PW")),
                password: Some(Secret::new("hunter2")),
                path: out.clone(),
            },
            watch::channel(false).1,
            move |line| sink.lock().expect("lock").push(line),
        )
        .await
        .expect("ran");
        assert_eq!(*seen.lock().expect("lock"), ["hunter2", "two"]);

        let failed = super::run(
            Run {
                program: PathBuf::from("/bin/sh"),
                command: command(&["-c", "exit 3"], None),
                password: None,
                path: out.clone(),
            },
            watch::channel(false).1,
            |_| {},
        )
        .await
        .expect_err("failed");
        assert!(failed.message.contains("exited with 3"), "{failed}");

        let (stop, cancel) = watch::channel(false);
        let slow = tokio::spawn(super::run(
            Run {
                program: PathBuf::from("/bin/sleep"),
                command: command(&["30"], None),
                password: None,
                path: out.clone(),
            },
            cancel,
            |_| {},
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = stop.send(true);
        let result = tokio::time::timeout(Duration::from_secs(5), slow)
            .await
            .expect("stopped in time")
            .expect("joined");
        assert_eq!(result.expect_err("cancelled").kind, ErrorKind::Cancelled);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
