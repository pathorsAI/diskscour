<p align="center">
  <img src="assets/logo.svg" width="116" height="116" alt="DiskScour">
</p>

<h1 align="center">DiskScour</h1>

<p align="center">A fast, native macOS disk analyzer that finds — and reclaims — the gigabytes of build junk on your dev machine.</p>

---

## The problem

Your disk fills up and you have no idea where it went. On a dev machine the answer is
almost always **regenerable build caches** — `node_modules`, Rust `target/`, `.next`,
`DerivedData`, `.gradle` — scattered across dozens of projects and git worktrees. They
add up to tens of gigabytes you can delete and recreate any time, but Finder won't show
you that, and `du` won't tell you which ones are safe to nuke.

**DiskScour scans a folder in seconds, shows you exactly what's eating the space, and lets
you move the regenerable caches to the Trash in one click.**

## Demo

Point it at a folder and it tells you how much is reclaimable — here, **59.8 GB of 62.3 GB**
is just build caches, grouped by ecosystem and safe to delete:

![DiskScour — dev caches](assets/demo-caches.png)

Browse what's actually there as a tree (sorted biggest-first) or a treemap — click to drill
in, hover for details:

![DiskScour — treemap](assets/demo-treemap.png)

> Caches are matched in context (a `target/` only counts next to a `Cargo.toml`, etc.) and
> nested caches are de-duplicated, so the "reclaimable" number is honest. Selected items go
> to the **macOS Trash** — recoverable, never a hard delete, always behind a confirmation.

## Run it

Needs a [Rust toolchain](https://rustup.rs).

```sh
git clone https://github.com/pathorsAI/diskscour
cd diskscour
cargo run --release
```

Prefer a double-clickable app in your dock? Build `DiskScour.app`:

```sh
cargo install cargo-bundle --locked
cargo bundle --release   # → target/release/bundle/osx/DiskScour.app
```

### From the terminal

No window, just the numbers:

```sh
diskscour scan ~/Github
```

Everything is available as JSON for scripting — `--json` works on `scan`, `caches`,
`status` and `trash`:

```sh
diskscour caches ~/Github --json --min 1000000000   # every cache over 1 GB
diskscour status                                    # what's already indexed
diskscour trash ~/Github/some/target                # previews; add --yes to do it
```

`diskscour --help` lists the lot.

## Scan once, query often

The first scan of a folder is a full walk. After that DiskScour keeps a small
index in `~/Library/Caches/com.pathors.diskscour/`, and asks the macOS FSEvents
log what changed since — so a repeat scan only walks the directories that
actually moved.

```
~/Github, 140.9 GB across 5.1M files

  first scan   67.51s   full
  rescan        0.66s   events · 4 directories changed
```

Two things keep that honest. Any doubt about the event history — dropped events,
a purged log, a network volume, a root reached through a symlink — falls back to
a slower strategy rather than pruning on a guess. And every result says which
strategy produced it, so the numbers never arrive without their provenance.

The one thing incremental refresh can miss is a file that grows *in place*
without its directory changing. `--full` corrects that on demand, and an index
older than a week does a full scan on its own.

## Use it from Claude Code

DiskScour speaks MCP, so a coding agent can read the scan results and clean up
without shelling out and parsing text:

```sh
claude mcp add diskscour -- /usr/local/bin/diskscour mcp
```

That adds six tools: `ds_status`, `ds_scan`, `ds_caches`, `ds_top`, `ds_tree`
and `ds_trash`. Reads are served from the index rather than by scanning, so
asking is cheap.

Deleting is the part worth being careful about, so the guards live in the code
rather than in a tool description:

- `ds_trash` **previews by default** and deletes nothing without `confirm: true`.
- Only recognised regenerable caches are accepted. Anything else needs
  `allow_any`, which is documented as something the user has to ask for by name.
- Targets must sit inside an indexed root. The scan root itself, `$HOME`, paths
  containing `..`, and symlinks resolving out of the root are refused outright.
- Every target is **re-checked against the filesystem at the moment of deletion**,
  so a stale index can't cause the wrong thing to go.
- Deletion always means the macOS Trash. There is no hard-delete path.

## License

MIT © Pathors
