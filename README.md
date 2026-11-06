# Forge

A source-based package manager for Linux. Point it at a Git repository; it
builds the software in a sandbox, installs it into its own isolated directory,
and can remove it again without leaving anything it installed behind.

(Working name; it is likely to change.)

```
forge install niri foot            # by name, from the Forge package repository
forge build https://github.com/fastfetch-cli/fastfetch   # any git link, no recipe needed
forge sync                         # refresh the package repository
forge tap add <git-url> | list | update | remove <name>   # more repositories
forge list | info <pkg> | verify [pkg]
forge update <pkg> | upgrade
forge remove <pkg> [--dry-run] [--keep-deps] [--keep-data] | autoremove
forge integrate <pkg> [--dry-run] | unintegrate <pkg>   # display-manager session files (sudo)
forge clean [--all] | search <q> | doctor | env
```

Forge is rootless: everything lives under `$FORGE_ROOT`
(default `~/.local/share/forge`).

**Commands work right after install.** Each installed command is also linked
into `~/.local/bin` (if it exists, is on your PATH and is writable), so there
is nothing to configure. Those links are tracked like everything else and
removed with the package. An existing file there is never replaced (Forge
warns, and the command stays available through `$FORGE_ROOT/bin`).
`FORGE_LINK_DIR=<dir>` picks another directory; set it empty to disable.
`forge relink [pkg]` adds the links to packages installed before this existed.
If you have no suitable directory, put Forge's own `bin/` on your PATH instead:
`eval "$(forge env)"`.

**Speed.** A source build can't match downloading a binary: a first install
is git (a few seconds) plus compiling. For fastfetch on an 8-core laptop that
is about 4s of git and 14s of compile. Forge's own work is well under a
second. To keep it as fast as it can be, Forge fetches git history without
file contents and then only the files of the one commit it builds (a cold
fetch of fastfetch dropped from 11.6s to about 4.8s, and the cache from 32MB
to 9MB). On a terminal a live line shows the build's progress, and `-v`
shows the full output.

## What an install looks like

```
  ✓ fetch github.com/fastfetch-cli/fastfetch     3.0s
  ✓ unpack fastfetch                             1.8s

Installing 1 package

  ◆ fastfetch 2.70.0+gitcad01fd2
      source   github.com/fastfetch-cli/fastfetch @ cad01fd2
      recipe   auto-detected · cmake
      runtime  config, data, cache and state kept in ~/.local/share/forge/data/fastfetch/

  sandbox  bubblewrap · no network while building · your home directory is hidden

Proceed? [Y/n]
Building
  ✓ configure                                    2.3s
  ✓ compile                                      12.2s
  ✓ stage files                                  0.1s
  ✓ install                                      0.2s

  ✓ Installed fastfetch 2.70.0+gitcad01fd2  46 files
  ready fastfetch, flashfetch  (linked into ~/.local/bin, already on your PATH)
```

On a terminal the current step animates with the build's own latest line
(ninja's `[37/142] ...`) and the output is colored. Piped or logged output is
plain text without escape codes. `NO_COLOR` turns color off, `FORGE_COLOR=always`
forces it, and `FORGE_ASCII=1` swaps the symbols for ASCII.

The plan stays short on purpose, but nothing is hidden that you'd need to
review: **commands supplied by a recipe are always listed in full** (`runs $ ...`),
since that is the text that decides what runs. The standard commands of a
build system (`cmake --build ...`) are listed with `--details`. Enter accepts
the prompt (`[Y/n]`); without a terminal, pass `--yes`.

A missing host tool or library is reported before anything is downloaded, for
the package and every Forge package it needs, all at once.

The other commands share that style (`list`, `info`, `verify`, `search`,
`remove`, `autoremove`, `clean`, `tap`, `doctor`). For example:

```
$ forge list

  NAME       VERSION                      BUILD      KIND        INSTALLED
  fastfetch  2.70.0+gitcad01fd2           cmake      explicit    2026-11-05
  hyprlang   0.6.8+git9508458b-dc1c83b1e  cmake      explicit    2026-11-05
  hyprutils  0.14.3+git7e1f6920           cmake      dependency  2026-11-05

$ forge remove hyprlang

Removing hyprlang and what only it needed

  ✓ Removed hyprlang 0.6.8+git9508458b-dc1c83b1e  5 files, 1 link
  ✓ Removed hyprutils 0.14.3+git7e1f6920  (no longer needed)  48 files, 1 link
```

`remove` lists anything outside the package it is about to delete (a data
directory with files in it, session files in system directories) and asks
first; `--dry-run` shows the whole plan without changing anything.

## Compilers and build tools: nothing to install first

Go and Rust projects need a recent compiler, Meson projects need Meson and
Python, and the ones on your machine may be missing, broken or too old. Forge
doesn't make you sort that out: when a build needs one of them and the
machine's doesn't qualify, Forge downloads the official release, checks it, and
builds with that. "Qualify" means it really works: Python is tested by
importing the modules builds use, since a system whose interpreter and
standard library don't match counts as broken. gh, for example, needs Go
1.27; on a machine with a broken `go`:

```
compiler go 1.27.3 · downloads 67.3 MB from go.dev, kept for later builds  (this machine's go is missing or doesn't run)

Toolchains
  ✓ download go 1.27.3                          5.3s
```

* **Where it comes from.** Go from `go.dev/dl`, Rust (rustc, cargo, rust-std)
  from `static.rust-lang.org`, Meson's source from PyPI, and Python from
  python-build-standalone (relocatable CPython builds), over HTTPS only. The
  download must match the SHA-256 the publisher lists, or it is deleted and the
  install stops.
* **Where it lives.** `$FORGE_ROOT/toolchains/<tool>-<version>/`, a cache Forge
  owns. It is not part of any package, so removing a package never removes it
  and updates don't rebuild things when it changes. Later builds reuse it with no
  network. `forge toolchain list | install go [version] | remove go-1.27.3`.
* **What the project asks for** is read from its own files: `go.mod` (`go` /
  `toolchain`), `Cargo.toml` (`rust-version`, edition 2024) and
  `rust-toolchain.toml`, `meson.build` (`meson_version`). The newest stable
  release that satisfies it is used. Nightly toolchains are not supported.
  Python is added for Meson builds and for recipes that list `python3` under
  `tools`.
* **Builds stay sandboxed.** The toolchain is mounted read-only; Go is told
  never to fetch another toolchain (`GOTOOLCHAIN=local`).
* **Policy**, via `FORGE_TOOLCHAINS`: `auto` (default: the machine's compiler if
  it works and is new enough), `managed` (always Forge's own: the same compiler on
  every machine), `host` (never download; fail with an explanation).

This stops at Go, Rust, Meson and Python today: compilers can't be built into
the `forge` program itself, and a Go compiler is built by an older Go, so
something published has to start the chain. C and C++ builds (gcc, cmake, ninja,
make) and the GNU autotools still use the machine's, so a machine whose
`autoconf` is broken can't build projects that ship no generated `configure`.

**For a distribution that ships Forge**: it needs `git`, `bubblewrap`, `curl`,
`tar`, `xz` and `cp` at runtime. Toolchains can be preinstalled by unpacking
them into `$FORGE_ROOT/toolchains/` (or `forge toolchain install go` at image
build time), after which nothing is downloaded and builds work offline. Setting
`FORGE_TOOLCHAINS=managed` makes every build use exactly those compilers.

## Guarantees

* **One directory per package**: `store/<name>/<version>-<commit8>/`.
* **Forge owns what it installs.** Every file is hashed into a manifest
  (database + `.forge/manifest.json` inside the package). `forge verify`
  detects missing, modified and unexpected files.
* **Clean removal.** `remove` deletes the package directory and the symlinks
  Forge created, and nothing else. A symlink is deleted only if it still
  points where Forge recorded it; the directory is deleted only at its exact
  expected path, via an atomic rename into `trash/` first.
* **Never overwrites what it doesn't own.** Installing refuses to replace
  anything in `bin/` that Forge didn't create, or that another package owns.
* **Builds can't escape their prefix.** If a build's `make install` tries to
  write to `/etc`, `/usr` or anywhere outside the package directory, the
  install is rejected. Setuid files and symlinks pointing outside the package
  are rejected too.
* **Atomic.** A failed build or install leaves the installed state unchanged.
  Updates keep the old version active until the new one is fully in place.
  With dependencies, *every* package in the plan is built before *any* is
  installed, so a failing dependent never leaves its dependencies behind.

Files a program creates while running are covered too, within limits: see the
next section.

## Where a program keeps its files

Each exported command is a small launcher script, not a bare symlink. By
default it points the program's `XDG_CONFIG_HOME`, `XDG_DATA_HOME`,
`XDG_CACHE_HOME` and `XDG_STATE_HOME` at `$FORGE_ROOT/data/<package>/`, so a
program that follows the XDG convention writes its configuration there
instead of `~/.config`, `~/.local/share` and `~/.cache`. `forge remove`
deletes that directory with the package (it lists it and asks first if the
program created files in it; `--yes` to skip, `--keep-data` to keep it for a
later reinstall). Updates never touch it. `XDG_RUNTIME_DIR` is left alone.

Checked with fastfetch: `fastfetch --gen-config` wrote its file into
`data/fastfetch/config/`, nothing appeared in `~/.config`, and removal
deleted it.

**The catch: environment variables are inherited.** Everything a program
starts sees the redirected variables too. That is harmless for tools like
fastfetch, but a compositor, terminal or shell would make every program you
start from it use the package's private config instead of yours. So the
behaviour is per package, set in the recipe or with `--isolate`:

| `isolate` | effect |
|---|---|
| `xdg` (default) | the four XDG directories point into the package's data directory |
| `home` | the same, and `HOME` too (catches programs that ignore XDG; breaks more) |
| `none` | environment untouched; use `env` / `args` to point only the program's own settings |

```toml
[run]
isolate = "none"
dirs = ["config/niri"]                       # created under {data} before starting
[run.env]
NIRI_CONFIG = "{data}/config/niri/config.kdl"
[run.seed]                                   # copied in if missing, never overwritten
"config/niri/config.kdl" = "share/niri/default-config.kdl"
# args = ["--config", "{data}/config/hypr/hyprland.conf"]   # placed before your arguments
# args_commands = ["Hyprland"]               # ...for these commands only (default: all)
```

`{data}` is `$FORGE_ROOT/data/<package>`, and `{dep:NAME}` is the directory of
the Forge package NAME (for a QML or plugin path, say). The niri recipe uses this: niri reads
`NIRI_CONFIG` and removes it from its own environment, so programs started
from niri don't inherit it. The Hyprland recipe passes `--config` instead
(unverified, like the rest of that recipe).

Limits, honestly: this only helps programs that honour XDG or the settings you
point at. Programs that hard-code `~/.something` still write there unless you
use `home`, and a compositor on `none` can still leave caches or logs in
`~/.cache` (Hyprland writes crash reports there), which Forge cannot redirect
without the inheritance problem above. Forge doesn't track files outside
`data/<package>`, so those are not removed.

## Security model

Forge refuses to run as root. Builds run under Bubblewrap:

* read-only `/usr` and `/etc`; private `/tmp`, `/proc`, `/dev`;
* no `$HOME`, no other Forge packages; read-write only the job's own directories;
* **no network** except in the *fetch* phase (`cargo fetch`, `go mod download`, …);
* cleared environment (only toolchain variables such as `CC`, `CXXFLAGS`,
  `RUSTFLAGS` pass through), wall-clock limit of 2 hours, capped file size,
  no core dumps, dies with its parent.

Sources are pinned: the revision is resolved to a full commit hash, the
checkout is verified against it, and the hash is recorded. A recipe can pin
`commit` explicitly. Before building, Forge prints the commit, the recipe and
every command that will run, and asks for confirmation (`--yes` to skip).

The sandbox protects your system *during the build*. The installed program
afterwards runs with your normal permissions, like any software you install.
Review what you install. `--insecure-no-sandbox` exists for machines where
Bubblewrap can't run and prints a warning.

Memory/CPU cgroup limits are not implemented yet.

## Dependencies between Forge packages

A recipe can name other Forge packages it needs:

```toml
[package]
deps = ["hyprutils"]          # needed to build and to run
[build]
deps = ["hyprwayland-scanner"]  # needed only while building (code generators)
```

* **Installing** builds missing dependencies first (from their recipes), as
  one plan with one confirmation. Dependencies are marked *dependency*;
  packages you asked for are *explicit*. Asking for a dependency by name
  promotes it to explicit.
* **Building** sees the dependencies read-only at their real store paths:
  tools on `PATH`, `PKG_CONFIG_PATH`, `CMAKE_PREFIX_PATH`, `FORGE_DEPS`. Even a
  dependency built in the same plan (not installed yet) is mounted at the path
  it will have. Installed binaries and libraries get an RPATH to their
  dependencies' libraries, so nothing needs `LD_LIBRARY_PATH`.
* **Removing** a package that others need is refused. Removing a package also
  removes automatic dependencies nothing else needs (`--keep-deps` to keep
  them; `forge autoremove` cleans up later).
* **Updating** a dependency rebuilds everything built against it. A package's
  version id includes a hash of its dependencies' versions, so the rebuilt
  dependent gets a new directory and the swap is atomic. `forge verify`
  reports dependents that were built against an older version.
* Cycles and missing recipes are reported before anything is built.
* `[build] pkgconfig` lists *host* libraries only. Forge packages bring their own.
* Dependencies on packages that aren't installed yet require the sandbox.

## Display-manager sessions (`forge integrate`)

Compositors need a session file in a root-owned directory
(`/usr/share/wayland-sessions`, `/usr/share/xsessions`) to appear in the login
screen. This is the one thing Forge does outside its own root, so it is a
separate, explicit step:

```
forge integrate hyprland --dry-run   # shows exactly what would be installed
forge integrate hyprland             # asks, then uses sudo for the file copy only
forge unintegrate hyprland
```

* Forge itself still refuses to run as root. Only a single `sudo install` /
  `sudo rm` is run, with the exact command printed first; if the directory is
  already writable, no sudo is used.
* The session file is installed as a **copy** named `forge-<name>.desktop`
  with `(Forge)` added to its `Name`, so it can't be confused with the
  distribution's own entry. `Exec=` points to a small wrapper in
  `$FORGE_ROOT/integration/<pkg>/` that puts Forge's `bin` on `PATH` and runs
  the program through its stable link, so it keeps working after updates.
* Every installed file is recorded with its hash. `forge verify` checks them.
  Removal deletes a file only if it is still byte-identical to what Forge
  wrote; otherwise it is left alone and reported.
* `forge remove <pkg>` removes the session file first (asking for
  confirmation, `--yes` to skip) and refuses to remove the package if that
  fails, so you can't end up with a session entry for a deleted program.
* It refuses to overwrite a file Forge doesn't own, and refuses a session file
  whose command the package doesn't provide (for example one that expects a
  helper script or systemd unit the recipe doesn't install).

## Two ways to install

1. **`forge install <name>`: from the Forge package repository.** It takes
   names only, and only from the repository
   ([Forge-Packages](https://github.com/noxilith/Forge-Packages), set up
   automatically the first time you install something). Everything in it is
   built from source, and its dependencies come from it too.
2. **`forge build <git link>`: any git repository, with nothing else.** Forge
   detects the build system (Cargo, CMake, Meson, Autotools, Go, Make) and
   builds it. Good for small, self-contained projects like fastfetch.

The two meet at the link: **if the repository has a recipe for that project,
`forge build` uses it.** `forge build https://github.com/YaLTeR/niri` (also with
`.git`, `git@github.com:...` or a trailing slash) gives the same result as
`forge install niri`, including the dependencies. `--ignore-repo` builds a link
with auto-detection only, and `--rev` still overrides the recipe's revision.

## The package repository

[Forge-Packages](https://github.com/noxilith/Forge-Packages) is a git repository
of recipes, in two directories:

* `recipes/`: written and kept up by hand (niri, the Hyprland family, foot, the
  GitHub CLI, ...).
* `foundry/`: generated from [Void Linux](https://voidlinux.org)'s package
  templates by `forge-import`, so Forge's catalogue follows Void's. About 5,200
  of Void's 9,700 templates convert (those with one https source archive and a
  CMake, Meson, Cargo, Go, Autotools or Make build); the rest are listed with
  reasons in `foundry/REPORT.md`. These recipes have **not been built**.

A recipe is `<name>.toml`, or `<name>/recipe.toml` next to a `patches/` directory.
Besides a git `repo`, a recipe can name a release `archive` with a mandatory
`sha256`, and `patches` to apply before building (inside the sandbox).

```
forge search hypr          # searches the repository, your recipes and other repositories
forge sync                 # fetch newer recipes
forge update <pkg> --refresh-recipe   # rebuild with the repository's current recipe
forge tap add <git-url>    # another repository with the same layout
```

`FORGE_REPO=<git-url>` replaces the official repository (an empty value means
none), for your own distribution's or a mirror.

* Lookup order: `$FORGE_ROOT/recipes/` (your own) first, then each repository's
  `recipes/`, then its `foundry/`. Dependencies named in a recipe are looked up the
  same way, so a hand-written recipe always beats an imported one.
* An update normally uses the recipe saved when the package was installed, so a
  tap change never alters installed packages silently; `--refresh-recipe` opts in.
* Recipes are build instructions that run in the sandbox, but they are still
  code you choose to run, so add only taps you trust. Forge prints the recipe
  source (tap and commit) and every command before building.

## Recipes

A recipe is TOML, found via `--recipe`, a name in `$FORGE_ROOT/recipes` or a
tap, or a `forge.toml` in the repository itself.

```toml
[package]
name = "niri"
repo = "https://github.com/YaLTeR/niri"
rev  = "v25.08"          # tag/branch/commit; default: default branch
commit = "abc123"        # optional: must match, else refuse

[build]
system = "cargo"         # omit to auto-detect; "custom" + steps for the rest
tools = ["cargo"]        # host tools required before building
pkgconfig = ["libinput", "wayland-server >= 1.22"]
configure_args = []      # passed to the backend's configure/build command
# steps = ["./bootstrap.sh", "make -j$FORGE_JOBS", "make install DESTDIR=$FORGE_STAGE PREFIX=$FORGE_PREFIX"]

[install]
exports = ["bin/*"]      # linked into $FORGE_ROOT/bin (default: bin/*)
post = ["install -Dm644 resources/x.desktop \"$FORGE_STAGE$FORGE_PREFIX/share/x.desktop\""]
```

Build commands see `$FORGE_SRC $FORGE_OBJ $FORGE_PREFIX $FORGE_STAGE
$FORGE_JOBS`. A build must install into `$FORGE_STAGE$FORGE_PREFIX` (i.e.
honour `PREFIX` and `DESTDIR`); the backends do this for you.

Build dependencies are *host* packages (compilers, `-dev` libraries). Forge
checks for them and tells you what is missing; it doesn't install them.

## Layout

```
$FORGE_ROOT/
  store/<name>/<ver>-<commit8>/   immutable package contents + .forge/manifest.json
  current/<name> -> store/...     active version (relative symlink)
  bin/<cmd> -> ../current/<name>/bin/<cmd>
  sources/  builds/  trash/  db/forge.sqlite  lock
  recipes/   your own recipes          taps/<name>/   cloned taps
  data/<pkg>/   a package's config/data/cache (see "Where a program keeps its files")
```

## Code

| crate | role |
|---|---|
| `forge-core` | paths, manifest, recipe, build-system detection, utilities |
| `forge-store` | SQLite ownership DB, stage validation, atomic install/update, verified remove, verify |
| `forge-build` | git mirror/checkout, build backends (one per build system), sandbox, orchestration |
| `forge-cli` | the `forge` binary |
| `forge-import` | `forge-import void <void-packages> --out <dir>`: parses Void templates (never runs them) into recipes |

New build systems implement the small `Backend` trait in
`forge-build/src/backends.rs`: it only *describes* commands; the sandbox is
the single place anything is executed.

## Development

```
cargo test        # unit tests, store tests, and end-to-end tests with a real git repo and sandbox
```

The end-to-end tests exercise the real pipeline (git → sandboxed build →
install → run → update → remove) and compare the installed state before and
after. If your machine has no `cc` (only `gcc`), export `CC=gcc` and
`CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=gcc`; Forge passes these into
sandboxed builds.

## Status / roadmap

Working: install, remove, list, info, update, upgrade, autoremove, verify,
clean, search, doctor, and dependencies between Forge packages.

Tested against real projects: fastfetch (CMake), niri (Cargo), the GitHub CLI
(Go, with a Forge-provided toolchain), and the Hyprland libraries (dependency
chains, build-only tools, RPATH linking, cascade removal). Taps were tested with
the real recipes repository: installing aquamarine by its GitHub URL used the
tap's recipe and built its dependencies; fastfetch's URL, which no recipe
describes, installed with auto-detection.

`forge integrate` is tested end-to-end against a user-writable stand-in
directory (install, wrapper execution, update, tamper detection, removal).
The `sudo` path itself has **not** been run; only the command syntax was
checked. niri's session file runs `niri-session`, which the niri recipe does
not install (it needs a systemd user unit), so `forge integrate niri` will
refuse with an explanation until that is added.

**The imported Void recipes have never been built.** They are generated by
parsing templates, and a recipe's header lists what could not be carried over
(hooks such as `pre_configure`, compiler flags, dependencies with no recipe).
Expect failures; each one is a fix to the importer or to a recipe.

**The hand-written recipe set is mostly untested.** Forge-Packages has about 50 recipes (the
Hyprland family and the libraries it needs, foot, fuzzel, wl-clipboard, grim,
mako, niri, the GitHub CLI and more). About 35 of them built in my test runs;
the rest were fixed after failing and never rebuilt, cannot build on the machine
they were written on, or were never tried. **Hyprland itself has never been
built.** The status of each recipe is in the Forge-Packages README. If a build
fails for you, that is the expected way to find the next fix.

Next: multiple installed versions and rollback (the layout supports it), build
cache, cgroup resource limits, scanning build files to list the host libraries
a project needs.

## License

MIT. See [LICENSE](LICENSE).
