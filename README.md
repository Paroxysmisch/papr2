# papr2

A small research-paper manager written in Rust with [gpui-kit](https://gpui-kit.com).

## Features

- **Library**: drag and drop PDFs (or whole folders) onto the window, or use the **Add** button.
  Each PDF is copied into `Library/` once. Importing the same file again is detected by content.
- **Google Scholar**: search from the sidebar, pick a result, review and edit its BibTeX (fetched
  from Google Scholar, with the abstract and links added), and save it to your library. Then
  download the PDF from the link shown and drop it into papr2. You can also attach or replace the
  PDF later from the paper's details pane. A paper that has a PDF but no BibTeX can be looked up
  with **Find on Google Scholar**.
- **Metadata**: the list shows each paper's name, authors and abstract. Papers missing their PDF or
  BibTeX are flagged with a warning.
- **Collections** (e.g. one per project): you can add the same paper to several collections without
  copying anything. Drag papers from the list onto a collection, tick it in the details pane, or drop
  PDFs straight onto a collection. **Export BibTeX** writes the collection's bibliography,
  sorted by title, to `Collections/<name>/references.bib`; the notification has a button to open it.
- **Rename** a paper in the details pane. This renames its PDF and BibTeX files.
- **Open PDF** opens the file in the system's default PDF viewer (double-clicking a row does the same).
- **Search** filters papers by name, author or abstract.
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
    Attention Is All You Need.pdf      a paper is a name with a .pdf, a .bib, or both
    Attention Is All You Need.bib      its metadata: title, authors, abstract, links, ...
    BERT.bib                           BibTeX only: PDF not attached yet
  Collections/
    Thesis/                            one folder per collection
      Attention Is All You Need.pdf -> ../../Library/Attention Is All You Need.pdf
      Attention Is All You Need.bib -> ../../Library/Attention Is All You Need.bib
      references.bib                   written by "Export BibTeX"
  papr2-cache.toml                     remembers file identities; safe to edit or delete
```

A paper's `.bib` file is ordinary BibTeX that you can edit in any text editor (the details pane has
an **Edit .bib** button). Besides the usual fields, papr2 uses `abstract`, `url` (the publication
page) and `pdf` (a link to the PDF). BibTeX tools ignore fields they don't know. papr2 writes a
`.bib` only when you save one from Google Scholar; after that, the file is yours.

Collections contain relative symbolic links into `Library/`, so the whole `papr2` folder can be
moved or synced as a unit. papr2 checks the folder every second, so you can manage things from
your file manager too:

| In the file manager…                         | …in papr2                                          |
| -------------------------------------------- | -------------------------------------------------- |
| rename a PDF or `.bib` in `Library/`         | the paper is renamed; its other file follows*      |
| rename a link inside a collection            | the paper is renamed everywhere                    |
| give a PDF the same name as a `.bib`         | they become one paper (a way to attach a PDF)      |
| rename / create / delete a collection folder | the collection is renamed / created / deleted      |
| delete a link from a collection              | the paper leaves that collection                   |
| copy a PDF into `Library/`                   | it appears in the library                          |
| copy a PDF into a collection folder          | it's moved into `Library/` and linked back         |
| delete a paper's files from `Library/`       | the paper and its links are removed                |

\* papr2 waits a few seconds first, in case you're renaming both files yourself.

`papr2-cache.toml` records each file's inode, size and modification time, so papr2 can tell a
rename apart from a deletion followed by a new file. If it's deleted, papr2 rebuilds it; a rename
made while papr2 wasn't running then can't be followed, and you'll see the PDF and `.bib` as two
papers with warnings. Nothing is deleted: rename either one to match the other to pair them again.

Libraries created by earlier versions (`~/.local/share/papr2`) are migrated automatically on first
launch. Files are copied, and the old `library.json` is renamed to `library.json.migrated`.

## Google Scholar search

Searching uses the [scholarly](https://scholarly.readthedocs.io/) Python library, run with
[uv](https://docs.astral.sh/uv/) (install: `curl -LsSf https://astral.sh/uv/install.sh | sh`).
papr2 keeps a small Python project in `~/.local/share/papr2/scholar/` (copied from `scholar/` in
this repository, with a pinned `uv.lock`). `uv` sets up its environment on the first search, which
can take a minute. Nothing is installed system-wide.

Google Scholar limits automated searches; if it refuses, papr2 says so, and you can try again
later. Its search results only include a snippet of each abstract. papr2 saves that snippet, and
you can edit it before saving or in the `.bib` file later.

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

Besides library and BibTeX tests, this runs headless UI tests that click through the real
interface (selection, Select mode, closing the side pane, attaching PDFs, exporting BibTeX).
They don't contact Google Scholar.
