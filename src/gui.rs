//! Desktop mode (`openxplore` with no arguments): mounts the drive at ~/Xbox360, opens the
//! file manager there and puts an icon in the system tray whose menu safely removes it.
//!
//! This process runs as the desktop user and never touches the disk itself. The mount runs
//! in a child `openxplore auto mount --control` (through pkexec when the udev rule has not
//! given the user access to the drive), and the two talk over the child's stdin/stdout:
//!   child -> "mounted" once ready; we -> "unmount"; child -> "busy" or "unmounted".
//! If this process dies, the child sees EOF on stdin and unmounts on its own.
//!
//! Settings live in ~/.config/openxplore/config.ini, created with defaults on first run.
use anyhow::{bail, Context, Result};
use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, StandardItem};
use ksni::{Category, Icon, ToolTip, Tray};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Sender};

const ICON_SIZE: i32 = 64;

const DEFAULT_CONFIG: &str = "\
[mount]
# Folder the drive opens in. ~ means your home folder.
# It must be empty or not exist yet; changes apply the next time you start OpenXplore.
path = ~/Xbox360
";

fn config_file() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"));
    base.join("openxplore/config.ini")
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

/// Mount folder from the config text; anything missing falls back to the default.
fn mount_path(config: &str) -> PathBuf {
    let value = config
        .lines()
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| k.trim() == "path")
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
        .unwrap_or("~/Xbox360");
    match value.strip_prefix('~') {
        Some(rest) => home().join(rest.trim_start_matches('/')),
        None => PathBuf::from(value),
    }
}

pub fn run() -> Result<()> {
    let r = session();
    if let Err(e) = &r {
        notify(&format!("{e:#}"));
    }
    r
}

fn session() -> Result<()> {
    let cfg = config_file();
    if !cfg.exists() {
        std::fs::create_dir_all(cfg.parent().unwrap())?;
        std::fs::write(&cfg, DEFAULT_CONFIG)?;
    }
    let mp = mount_path(&std::fs::read_to_string(&cfg)?);
    std::fs::create_dir_all(&mp).with_context(|| format!("creating {}", mp.display()))?;
    // Mounting over a folder hides its contents until unmount, so a typo such as `path = ~`
    // would make the user's files seem to vanish.
    if std::fs::read_dir(&mp)?.next().is_some() {
        bail!("{} is not empty. Choose an empty folder in {}.", mp.display(), cfg.display());
    }
    let exe = std::env::current_exe()?;
    let mut cmd = if crate::find_drive().is_ok() {
        Command::new(&exe)
    } else {
        let mut c = Command::new("pkexec");
        c.arg(&exe);
        c
    };
    let mut child = cmd
        .args(["auto", "mount", "--control"])
        .arg(&mp)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting the mount helper")?;
    // Drain stderr on a thread: the helper logs per-operation errors there, and an unread
    // pipe would eventually block it. The text is kept to explain a failed start.
    let mut err = child.stderr.take().unwrap();
    let errors = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let mut out = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut stdin = child.stdin.take().unwrap();

    if out.next().and_then(|l| l.ok()).as_deref() != Some("mounted") {
        let status = child.wait()?;
        let _ = std::fs::remove_dir(&mp);
        if matches!(status.code(), Some(126 | 127)) {
            return Ok(()); // pkexec: the user cancelled the password prompt
        }
        let msg = errors.join().unwrap_or_default();
        let msg = msg.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("unknown error");
        bail!("{}", msg.trim_start_matches("Error: "));
    }

    open(&mp);
    let (tx, rx) = channel();
    let tray = DriveTray { folder: mp.clone(), tx };
    let handle = tray.assume_sni_available(true).spawn().context("creating the tray icon")?;
    // Wait for "Safely remove"; a busy drive keeps the icon so the user can retry.
    for () in rx {
        writeln!(stdin, "unmount")?;
        match out.next().and_then(|l| l.ok()).as_deref() {
            Some("busy") => notify("The drive is still in use. Wait for copies to finish and close programs using it, then try again."),
            _ => break,
        }
    }
    handle.shutdown().wait();
    drop(stdin);
    let ok = child.wait()?.success();
    let _ = std::fs::remove_dir(&mp);
    if !ok {
        bail!("Unmounting failed: {}", errors.join().unwrap_or_default().trim());
    }
    notify("All data is written. You can unplug the drive now.");
    Ok(())
}

struct DriveTray {
    folder: PathBuf,
    /// Signals the session loop that the user chose "Safely remove".
    tx: Sender<()>,
}

impl Tray for DriveTray {
    fn id(&self) -> String {
        "openxplore".into()
    }

    fn title(&self) -> String {
        "OpenXplore".into()
    }

    fn category(&self) -> Category {
        Category::Hardware
    }

    // Sent as pixels rather than a name: a fresh install's icon may not be in the
    // desktop's icon cache yet, which would leave a blank tray entry.
    fn icon_pixmap(&self) -> Vec<Icon> {
        let mut data = include_bytes!("../packaging/icons/openxplore.rgba").to_vec();
        data.as_chunks_mut::<4>().0.iter_mut().for_each(|px| px.rotate_right(1)); // RGBA -> ARGB, as StatusNotifierItem requires
        vec![Icon { width: ICON_SIZE, height: ICON_SIZE, data }]
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip { title: format!("Xbox 360 drive open in {}", self.folder.display()), ..Default::default() }
    }

    // Left click opens the folder; the menu is on right click.
    fn activate(&mut self, _x: i32, _y: i32) {
        open(&self.folder);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem { label: "Open folder".into(), activate: Box::new(|t: &mut Self| open(&t.folder)), ..Default::default() }.into(),
            StandardItem { label: "Open config".into(), activate: Box::new(|_: &mut Self| open(&config_file())), ..Default::default() }.into(),
            MenuItem::Separator,
            StandardItem {
                label: "Safely remove".into(),
                icon_name: "media-eject".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(());
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

fn open(path: &std::path::Path) {
    let _ = Command::new("xdg-open").arg(path).spawn();
}

/// Desktop notification, falling back to stderr when notify-send is missing.
fn notify(text: &str) {
    let sent = Command::new("notify-send").args(["-a", "OpenXplore", "-i", "openxplore", "OpenXplore", text]).status();
    if sent.is_err() {
        eprintln!("{text}");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn mount_path_from_config() {
        let home = super::home();
        assert_eq!(super::mount_path(super::DEFAULT_CONFIG), home.join("Xbox360"));
        assert_eq!(super::mount_path(""), home.join("Xbox360"));
        assert_eq!(super::mount_path("[mount]\npath = ~/Games/360 \n"), home.join("Games/360"));
        assert_eq!(super::mount_path("path=/mnt/xbox"), std::path::PathBuf::from("/mnt/xbox"));
        assert_eq!(super::mount_path("# path = /nope\npath ="), home.join("Xbox360"));
    }

    #[test]
    fn icon_is_64px_rgba() {
        let tray = super::DriveTray { folder: "/x".into(), tx: std::sync::mpsc::channel().0 };
        assert_eq!(ksni::Tray::icon_pixmap(&tray)[0].data.len(), (super::ICON_SIZE * super::ICON_SIZE * 4) as usize);
    }
}
