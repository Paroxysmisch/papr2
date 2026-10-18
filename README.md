# papr2

A small research-paper manager written in Rust with [gpui-kit](https://gpui-kit.com).

## Features

- **Library**: drag and drop PDFs (or whole folders) onto the window, or use the **Add** button.
  Each PDF is copied into `Library/` once. Importing the same file again is detected by content.
- **Collections** (e.g. one per project): you can add the same paper to several collections without
  copying the PDF. Drag papers from the list onto a collection, tick it in the details pane, or drop
  PDFs straight onto a collection.
- **Rename** a paper in the details pane. This renames the PDF file itself.
- **Open PDF** opens the file in the system's default PDF viewer (double-clicking a row does the same).
- **Search** filters papers by name.
- **Multi-select**: click **Select** (or Ctrl/Cmd-click, Shift-click for a range, Ctrl/Cmd+A for
  all) to select several papers. You can then add them all to a collection, start a new collection
  from them, remove them from the current collection, delete them, or drag them onto a collection.
- **Right-hand pane**: closes with its × button, Esc, or by clicking the sidebar or empty space in
  the list.
- **Light/dark mode** follows the system setting, including changes while papr2 is running.

## Storage: just files and folders

Everything lives in `~/Documents/papr2/` (override with `PAPR2_HOME`). There is no database:

```
~/Documents/papr2/
  Library/
    Attention Is All You Need.pdf      the only copy of each paper; file name = paper name
  Collections/
    Thesis/                            one folder per collection
      Attention Is All You Need.pdf -> ../../Library/Attention Is All You Need.pdf
  .papr2-cache.json                    hidden cache, safe to delete
```

Collections contain relative symbolic links into `Library/`, so the whole `papr2` folder can be
moved or synced as a unit. papr2 checks the folder every second, so you can manage things from
your file manager too:

| In the file manager…                         | …in papr2                                     |
| -------------------------------------------- | --------------------------------------------- |
| rename a PDF in `Library/`                   | the paper is renamed; collection links follow |
| rename a link inside a collection            | the paper is renamed everywhere               |
| rename / create / delete a collection folder | the collection is renamed / created / deleted |
| delete a link from a collection              | the paper leaves that collection              |
| copy a PDF into `Library/`                   | it appears in the library                     |
| copy a PDF into a collection folder          | it's moved into `Library/` and linked back    |
| delete a PDF from `Library/`                 | the paper and its links are removed           |

The cache only remembers file identities (inode/size/mtime) and content hashes, so papr2 can
tell a rename apart from a deletion. If it's deleted, a rename made while papr2 wasn't running
can't be followed, and links to the old name are dropped.

Libraries created by earlier versions (`~/.local/share/papr2`) are migrated automatically on first
launch. Files are copied, and the old `library.json` is renamed to `library.json.migrated`.

## Building (Fedora)

GPUI needs a few system development packages:

```sh
sudo dnf install fontconfig-devel freetype-devel libxkbcommon-x11-devel \
    libxkbcommon-devel wayland-devel libxcb-devel alsa-lib-devel
cargo run --release
```

## Tests

```sh
cargo test
```

Besides library tests, this runs headless UI tests that click through the real interface
(selection, Select mode, closing the side pane).
