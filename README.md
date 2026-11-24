<p align="center">
  <img src="packaging/icons/openxplore.svg" width="128" alt="OpenXplore logo">
</p>

<h1 align="center">OpenXplore</h1>

<p align="center">Free tool to read, write and mount Xbox 360 hard drives on Linux.</p>

---

Connect an Xbox 360 drive to your PC (a USB-to-SATA adapter works), then browse it, copy
games and saves off it, or copy new content onto it.

- Lists, copies off, copies on, creates and deletes files and folders
- Mounts the drive as a normal folder, so you can drag and drop files in Dolphin or any file manager
- Works on the drive itself or on a disk image made with `dd`
- Tested on a retail 500 GB drive: game data read and written back byte-for-byte

## Install

On Arch-based distributions, download the latest `.pkg.tar.zst` from the
[releases page](https://github.com/PiotrWieczorek98/openxplore/releases), then run:

```sh
sudo pacman -U openxplore-*.pkg.tar.zst
```

Or build the package yourself:

```sh
cd packaging
makepkg -si
```

On other distributions, install Rust and `fuse3`, then:

```sh
cargo build --release
sudo install -Dm755 target/release/openxplore -t /usr/local/bin
```

## Usage

### The easy way

1. Connect the drive (a USB-to-SATA adapter works).
2. Start **OpenXplore** from your application menu.
3. Your file manager opens the drive at `~/Xbox360`. Copy files as usual.
4. When you are done, right-click the OpenXplore icon in the system tray, choose
   **Safely remove**, and unplug the drive once the notification says so.

Left-clicking the tray icon reopens the folder. On GNOME, the tray icon needs the
AppIndicator extension.

To open the drive somewhere else, edit `~/.config/openxplore/config.ini` (also under
**Open config** in the tray menu). It is created on first start:

```ini
[mount]
path = ~/Xbox360
```

The folder must be empty or not exist yet. Changes apply the next time you start OpenXplore.

### Command line

Use `auto` to find the drive, or give its device (e.g. `/dev/sdb`, see `lsblk`).

```sh
openxplore auto partitions                   # find FATX partitions
openxplore auto ls /Content/0000000000000000
openxplore auto get /Content ./backup        # copy off the drive
openxplore auto put ./4D5309D6 /Content/0000000000000000
openxplore auto mkdir /Games
openxplore auto rm /Games                    # removes contents too
openxplore auto mount ~/xbox                 # Ctrl+C to unmount
openxplore -p compat /dev/sdb ls /           # other partition (default: data)
```

The package installs a udev rule that gives you access to Xbox 360 drives, so no `sudo` is
needed. If the drive was already connected while you installed, unplug it and plug it back in.
Without the package, prefix commands with `sudo`; the app menu entry asks for your password
instead.

> [!WARNING]
> Back up the drive before writing to it:
> `dd if=/dev/sdb of=xbox360.img bs=4M status=progress`

## Limitations

- Retail drive layout only; dev kit drives and Xbox 360-formatted USB sticks are not supported
- Names are limited to 42 characters, so copying a file with a long name fails
- Files are limited to 4 GiB, as on the console
- Cannot format a drive

## License

[GPL-3.0](LICENSE). Not affiliated with or endorsed by Microsoft. Xbox and Xbox 360 are
trademarks of Microsoft Corporation.
