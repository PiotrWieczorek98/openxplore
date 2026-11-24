<p align="center">
  <img src="packaging/icons/openxplore.svg" width="128" alt="OpenXplore logo">
</p>

<h1 align="center">OpenXplore</h1>

<p align="center">Free tool to read, write and mount Xbox 360 hard drives on Linux.</p>

---

Connect an Xbox 360 drive to your PC (a USB-to-SATA adapter works), then browse it, copy
games and saves off it, or copy new content onto it.

- Lists, copies off, copies on, creates and deletes files and folders
- Mounts a partition read-only so you can browse it in a file manager
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

Find your drive with `lsblk` (it has no Linux partitions, e.g. `/dev/sdb`). Accessing a
device needs root.

```sh
sudo openxplore /dev/sdb partitions               # find FATX partitions
sudo openxplore /dev/sdb ls /Content/0000000000000000
sudo openxplore /dev/sdb get /Content ./backup    # copy off the drive
sudo openxplore /dev/sdb put ./4D5309D6 /Content/0000000000000000
sudo openxplore /dev/sdb mkdir /Games
sudo openxplore /dev/sdb rm /Games                # removes contents too
sudo openxplore /dev/sdb mount ~/xbox             # read-only; fusermount3 -u ~/xbox
sudo openxplore -p compat /dev/sdb ls /           # other partition (default: data)
```

> [!WARNING]
> Back up the drive before writing to it:
> `sudo dd if=/dev/sdb of=xbox360.img bs=4M status=progress`

## Limitations

- Retail drive layout only; dev kit drives and Xbox 360-formatted USB sticks are not supported
- The mount is read-only; use `put`, `mkdir` and `rm` to write
- Files are limited to 4 GiB, as on the console
- Cannot format a drive

## License

[GPL-3.0](LICENSE). Not affiliated with or endorsed by Microsoft. Xbox and Xbox 360 are
trademarks of Microsoft Corporation.
