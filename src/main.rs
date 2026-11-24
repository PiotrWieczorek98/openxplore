mod fatx;
mod mount;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

/// Read/write Xbox 360 FATX drives and partition images.
#[derive(Parser)]
struct Cli {
    /// Block device (e.g. /dev/sdb) or image file
    device: PathBuf,
    /// Partition name (see `partitions`); defaults to "data", or the image itself
    #[arg(short, long)]
    partition: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List FATX partitions found on the device
    Partitions,
    /// List a directory
    Ls { #[arg(default_value = "/")] path: String },
    /// Copy a file or directory (recursively) from the drive to the host
    Get { src: String, dest: PathBuf },
    /// Copy a host file or directory (recursively) onto the drive, into directory DEST
    Put { src: PathBuf, #[arg(default_value = "/")] dest: String },
    /// Create a directory
    Mkdir { path: String },
    /// Delete a file or directory (recursively)
    Rm { path: String },
    /// Mount read-only via FUSE
    Mount { mountpoint: PathBuf },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let writes = matches!(cli.cmd, Cmd::Put { .. } | Cmd::Mkdir { .. } | Cmd::Rm { .. });
    let dev = OpenOptions::new().read(true).write(writes).open(&cli.device).with_context(|| format!("opening {}", cli.device.display()))?;
    let parts = fatx::find_partitions(&dev)?;
    if let Cmd::Partitions = cli.cmd {
        for (name, off, len) in &parts {
            println!("{name:<10} offset {off:#014x}  size {:>8} MiB", len >> 20);
        }
        return Ok(());
    }
    let want = cli.partition.as_deref();
    let Some(&(_, off, len)) = parts.iter().find(|p| want.map_or(p.0 == "data" || p.0 == "image", |w| p.0 == w)) else {
        bail!("partition {} not found; available: {:?}", want.unwrap_or("data"), parts.iter().map(|p| p.0).collect::<Vec<_>>());
    };
    let mut vol = fatx::Volume::open(dev, off, len)?;

    match cli.cmd {
        Cmd::Partitions => unreachable!(),
        Cmd::Ls { path } => {
            let e = vol.lookup(&path)?;
            for c in vol.read_dir(&e)? {
                let (y, m, d) = fatx::civil_from_days(fatx::fat_to_unix(c.mtime).div_euclid(86400));
                println!("{} {:>12} {y}-{m:02}-{d:02} {}{}", if c.is_dir() { 'd' } else { '-' }, c.size, c.name, if c.is_dir() { "/" } else { "" });
            }
            let (used, total) = vol.usage();
            eprintln!("{} / {} MiB used", used * vol.cluster_size >> 20, total * vol.cluster_size >> 20);
        }
        Cmd::Get { src, dest } => get(&vol, &vol.lookup(&src)?, &dest)?,
        Cmd::Put { src, dest } => {
            let dir = vol.lookup(&dest)?;
            put(&mut vol, &dir, &src)?;
        }
        Cmd::Mkdir { path } => {
            let (parent, name) = split(&path);
            let dir = vol.lookup(parent)?;
            vol.mkdir(&dir, name)?;
        }
        Cmd::Rm { path } => {
            let (parent, name) = split(&path);
            let dir = vol.lookup(parent)?;
            rm(&mut vol, &dir, name)?;
        }
        Cmd::Mount { mountpoint } => mount::run(vol, &mountpoint)?,
    }
    Ok(())
}

fn split(path: &str) -> (&str, &str) {
    let p = path.trim_end_matches('/');
    p.rsplit_once('/').unwrap_or(("", p))
}

fn get(vol: &fatx::Volume, e: &fatx::Entry, dest: &Path) -> Result<()> {
    // into an existing host directory -> keep the source name
    let dest = if dest.is_dir() && !e.name.is_empty() { dest.join(&e.name) } else { dest.to_path_buf() };
    if e.is_dir() {
        std::fs::create_dir_all(&dest)?;
        for c in vol.read_dir(e)? {
            get(vol, &c, &dest.join(&c.name))?;
        }
    } else {
        println!("{}", dest.display());
        vol.copy_out(e, &mut std::io::BufWriter::new(File::create(&dest)?))?;
    }
    Ok(())
}

fn put(vol: &mut fatx::Volume, dir: &fatx::Entry, src: &Path) -> Result<()> {
    let name = src.file_name().context("source has no file name")?.to_string_lossy();
    if src.is_dir() {
        let sub = match vol.find(dir, &name)? {
            Some(e) if e.is_dir() => e,
            Some(_) => bail!("{name} exists and is not a directory"),
            None => vol.mkdir(dir, &name)?,
        };
        for f in std::fs::read_dir(src)? {
            put(vol, &sub, &f?.path())?;
        }
    } else {
        println!("{}", src.display());
        let f = File::open(src)?;
        let len = f.metadata()?.len();
        vol.put(dir, &name, &mut std::io::BufReader::new(f), len)?;
    }
    Ok(())
}

fn rm(vol: &mut fatx::Volume, dir: &fatx::Entry, name: &str) -> Result<()> {
    let e = vol.find(dir, name)?.with_context(|| format!("{name}: not found"))?;
    if e.is_dir() {
        for c in vol.read_dir(&e)? {
            rm(vol, &e, &c.name)?;
        }
    }
    vol.remove(dir, name)
}
