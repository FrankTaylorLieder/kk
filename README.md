# kk

Per-shell Kubernetes context and namespace switching.

`kk` lets each terminal window point at its own context and namespace, including
several windows on the same cluster in different namespaces, without them affecting
each other. It doesn't need copies of your kubeconfig either. Every combination you
use is remembered, so you can jump back to it from any shell with a fuzzy picker.

## Why

- **Switching is global.** `kubectx`/`kubens` rewrite `current-context` in
  `~/.kube/config`, so switching in one window switches every window.
- **Copies drift.** Keeping several kubeconfig copies and pointing `KUBECONFIG` at
  them works, but the copies go out of date. Tools that refresh credentials or add
  clusters then write to whichever copy happens to be active.

`kk` instead gives each shell a tiny *overlay* file in front of your main config:

```sh
KUBECONFIG=<per-shell overlay>:~/.kube/config
```

The overlay holds only `current-context` and one context entry carrying the
namespace you picked. kubectl merges the two files, and the first file to define a
name wins, so the overlay's choice applies to that shell only.

When something writes back:

- **Namespace changes** go to the overlay, because that's where the context entry
  is defined.
- **Credential refreshes and cluster edits** go to `~/.kube/config`, which stays
  the single source of truth.

## Installation

Requirements:

- **Rust** (stable) to build.
- **`kubectl`** on `PATH`, used to list namespaces.
- **Optional: [`fzf`](https://github.com/junegunn/fzf)** for the pickers. Without
  it, `kk` uses a simple built-in picker.
- **bash or zsh.**

Install straight from GitHub:

```sh
cargo install --git https://github.com/FrankTaylorLieder/kk
```

This puts `kk` in `~/.cargo/bin`, which needs to be on your `PATH`. Run the same
command again to upgrade.

Or build from a local clone:

```sh
git clone https://github.com/FrankTaylorLieder/kk
cd kk
cargo install --path .
```

Then add the shell integration to your `~/.zshrc` or `~/.bashrc`:

```sh
eval "$(command kk init zsh)"            # or: kk init bash
```

To also define the short aliases `kp`, `kctx` and `kns`:

```sh
eval "$(command kk init zsh --aliases)"
```

The integration defines a `kk` shell function that wraps the binary. It's needed
because a program can't change its parent shell's environment. Without it, the
commands that switch context or namespace stop with instructions to set it up.

The integration also enables tab completion. In zsh, `compinit` must already be
loaded when `kk init` runs, so put the `eval` line after `compinit`, or after your
framework (e.g. oh-my-zsh) has loaded. If it isn't loaded, `kk init` prints a hint
and skips completion.

## Usage

```sh
kk context [CONTEXT]          # switch this shell's context (picker if omitted)
kk namespace [NAMESPACE]      # switch namespace within the current context
kk profile [NAME]             # switch to a saved profile (picker if omitted)
kk profile -                  # back to this shell's previous profile
kk profile name NAME          # name the current profile
kk profile list               # list profiles
kk profile current [--short]     # print the current profile's name (for prompts)
kk profile rm NAME            # remove a profile
kk import [--dry-run] [--overwrite]  # move entries other tools added into ~/.kube/config
kk exit [--force]             # this shell goes back to the plain ~/.kube/config
kk clean [--force]            # remove overlays left by shells that have exited
```

Short forms: `kk ctx`, `kk ns`, `kk p` and `kk profile ls`.

### Example

```sh
# window 1
$ kk ctx prod
kk: switched to prod/default
$ kk ns team-a
kk: switched to prod/team-a

# window 2: same cluster, different namespace; window 1 is unaffected
$ kk ns team-b
kk: switched to prod/team-b
```

### Profiles

A **profile** is a context and namespace combination, optionally with a different
user and extra environment variables. Every combination you switch to is recorded
in a global catalogue, so all your shells can pick from the same history while each
keeps its own setting.

- **Unnamed profiles** are shown and selected as `<context>/<namespace>`, e.g.
  `kk profile prod/team-a`.
- **Naming the current one:** `kk profile name prod-a`. After that, both `prod-a`
  and `prod/team-a` select it.
- **Name rules:** names can't contain `/` or whitespace, can't start with `-`, and
  can't be `name`, `list`, `ls`, `rm` or `current`.

`kk profile list` and the picker show the current shell's profile with a `*`. A
profile whose context no longer exists in `~/.kube/config` is marked `[stale]`.

```
$ kk profile list
   NAME         CONTEXT  NAMESPACE  USES  LAST USED
*  prod-a       prod     team-a     12    just now
   staging/qa   staging  qa         3     2h ago
```

#### Ordering

The picker and `kk profile list` put the most useful profiles first:

1. **Recent:** profiles used in the last hour come first, newest first.
2. **Everything else:** sorted by `use_count × weight`, where the weight drops as
   the last use gets older:

   | Last used | Weight |
   |---|---|
   | within a day | 1.0 |
   | within a week | 0.5 |
   | within a month | 0.25 |
   | older | 0.1 |

   So frequently used profiles win as their last use recedes.

Once all use counts add up to more than 1000, every count is scaled down by 10%.
Unnamed profiles whose count reaches zero are dropped, so old one-off combinations
fade out. Named profiles are never dropped.

When you type in fzf, it ranks entries by how well they match. Equally good matches
keep `kk`'s order.

#### Environment variables per profile

Edit the catalogue to have a profile export variables when you switch to it, such
as `AWS_PROFILE` for EKS credentials. When you switch away, they're unset again.

```yaml
# ~/.config/kk/profiles.yaml
profiles:
  - name: prod-a
    context: prod
    namespace: team-a
    env:
      AWS_PROFILE: prod
```

### Tab completion

TAB completes values as well as subcommands and flags:

| Command | Completes |
|---|---|
| `kk context <TAB>` | contexts from `~/.kube/config`, most used first |
| `kk profile <TAB>`, `kk profile rm <TAB>` | profile names, and `<context>/<namespace>` for unnamed ones, in picker order. Named profiles show their `<context>/<namespace>` as a description in zsh |
| `kk namespace <TAB>` | namespaces already used with the current context, then the cached namespace list |

- **Speed:** completion never waits on `kubectl`. If a context has no cached
  namespace list yet, or the list is stale, it's refreshed in the background, so
  the next TAB has it.
- **zsh aliases:** `kp`, `kctx` and `kns` complete like the commands they expand
  to.
- **bash aliases:** bash doesn't complete aliases, so use the `kk …` forms there.
- **Colons in bash:** context names containing `:`, such as EKS ARNs, can complete
  oddly, because bash splits words at `:`.

### Pickers

The pickers for `kk profile`, `kk context` and `kk namespace` use:

- **`fzf`** if it is on your `PATH`. Your `FZF_DEFAULT_OPTS` applies.
- **A built-in fuzzy picker** otherwise.

Set `KK_PICKER` to choose explicitly:

```sh
export KK_PICKER=builtin               # always use the built-in picker
export KK_PICKER="fzf --height 40%"    # any filter: candidates on stdin, choice on stdout
export KK_PICKER=sk                    # e.g. skim
```

Namespace lists are cached per context so the picker opens instantly. If the cache
is more than 5 minutes old, it's refreshed in the background for next time. Use
`kk namespace --refresh` to fetch a fresh list immediately.

### Clusters added by other tools

Tools that add clusters write their new entries into the first kubeconfig file,
which in a `kk` shell is the shell's overlay. Examples are
`aws eks update-kubeconfig` and `gcloud container clusters get-credentials`. They
usually also switch the shell to the new context. `kk` picks this up on your next
`kk` command in that shell:

```
$ aws eks update-kubeconfig --name foo
$ kk ns apps
kk: moved cluster 'foo', user 'foo', context 'foo' from this shell's overlay into ~/.kube/config (backup: ~/.kube/config.kk-backup)
kk: following the switch to context 'foo' made by another tool
kk: switched to foo/default
kk: switched to foo/apps
```

- **Where the entries go:** they move to `~/.kube/config`, where the tool would
  have put them without `kk`. That makes them available to every shell. The shell
  stays on the context the tool switched it to.
- **Context switches:** `kubectl config use-context` inside a `kk` shell is followed
  the same way.
- **Duplicates:** entries identical to ones already in `~/.kube/config` are just
  removed from the overlay.
- **Conflicts:** an entry with the same name as one in `~/.kube/config` but
  different contents is a conflict. For example, a tool re-wrote an existing
  cluster with a new endpoint.
  - **With a terminal:** `kk` shows what differs and asks before replacing.
    Values that may be secret, such as tokens and certificates, are never shown.
  - **Without one, or if you decline:** the entry stays in the overlay, with a
    warning.
- **Safety:**
  - `kk` writes `~/.kube/config` while holding kubectl's own lock file
    (`~/.kube/config.lock`).
  - It saves the previous version to `~/.kube/config.kk-backup` first.
  - It replaces the file atomically, keeping its permissions and the order of its
    entries.
  - It doesn't keep YAML comments.
- **`kk exit` and `kk clean`** import before deleting an overlay. They refuse to
  delete one that still holds entries, such as declined conflicts, unless you
  pass `--force`.

To review before anything is moved, set `KK_AUTO_IMPORT=0`, then import yourself:

```sh
kk import --dry-run      # show what would be moved, replaced or left
kk import                # import, asking about conflicts
kk import --overwrite    # import, replacing conflicting entries without asking
```

### Prompt

`kk profile current` prints the current shell's profile: its name if you've given it
one, otherwise `<context>/<namespace>`.

- **Without kk's overlay:** it shows the main kubeconfig's current context.
- **No context at all:** it prints nothing and exits with status 1.

It reads the shell's overlay directly, so it stays accurate even after you change
the namespace with `kubectl` itself.

It adds a `!` (e.g. `prod/team-a!`) when another tool has changed the shell's
overlay and `kk` hasn't picked it up yet. That means added entries or a switched
context. Your next `kk` command, or `kk import`, deals with it. See
[Clusters added by other tools](#clusters-added-by-other-tools).

Long context and namespace names can make for an unwieldy prompt. `--short` (`-s`)
abbreviates both parts of an unnamed profile:

```
$ kk profile current
public-local-se-sto-management/evroc-operator-system
$ kk profile current --short
pub-loc-se-sto-man/evr-ope-sys
```

How the abbreviation works:

- **Segments:** names are split at `-`, `_`, `.`, `:` and `/`.
- **Shortening:** working left to right, each segment is cut to the shortest
  prefix that still keeps the name unique, in the style of powerlevel10k's
  `truncate_to_unique`. Segments are never cut below 3 characters, so `se` and
  `az1` stay whole.
- **What it's unique among:**
  - the context, among the contexts in your kubeconfig
  - the namespace, among the namespaces your profiles use with that context. The
    cluster isn't queried, so this stays fast enough for a prompt.
- **Changes over time:** an abbreviation can get longer when you add a context or
  start using a similar namespace.
- **Named profiles** are always shown with their name in full.

Call the binary with `command kk` in prompts. It doesn't need the shell function,
and that skips the function's temp-file handling on every prompt.

zsh:

```sh
setopt PROMPT_SUBST
PROMPT='%F{cyan}$(command kk profile current --short 2>/dev/null)%f %~ %# '
```

bash:

```sh
PS1='\[\e[36m\]$(command kk profile current 2>/dev/null)\[\e[0m\] \w \$ '
```

`kk` also exports `KK_PROFILE` when it switches, for prompts that would rather
read a variable than run a command. Unlike `kk profile current`, it isn't updated by
changes made outside `kk`. Prompt tools that read the kubeconfig, such as
`kube-ps1`, see the merged config and should show the right context and namespace
without changes.

## Configuration

| Variable | Default | Purpose |
|---|---|---|
| `KK_PICKER` | `fzf` if installed, else `builtin` | Picker command (see [Pickers](#pickers)) |
| `KK_MAIN_KUBECONFIG` | `~/.kube/config` | The main kubeconfig that overlays sit in front of |
| `KK_CONFIG_DIR` | `~/.config/kk` | Holds `profiles.yaml` |
| `KK_RUNTIME_DIR` | `$XDG_RUNTIME_DIR/kk`, or `$TMPDIR/kk-<uid>` | Per-shell overlays and state (mode `0700`) |
| `KK_CACHE_DIR` | `~/.cache/kk` | Namespace cache |
| `KK_AUTO_IMPORT` | on | Set to `0` to stop `kk` importing entries other tools add to overlays until you run `kk import` |

Variables `kk` sets in your shell:

| Variable | Meaning |
|---|---|
| `KUBECONFIG` | `<overlay>:<main kubeconfig>` |
| `KK_PROFILE` | Current profile name, as of the last `kk` switch |
| *(profile `env`)* | Anything listed in the profile's `env` |

## Caveats

- **YAML comments in `~/.kube/config` are lost** when `kk` imports entries into
  it (see [Clusters added by other tools](#clusters-added-by-other-tools)). The
  previous version is kept in `~/.kube/config.kk-backup`.
- **Changing the namespace with kubectl directly**
  (`kubectl config set-context --current --namespace=…`) works and only affects
  the current shell. `kk profile current` reflects it immediately, but `KK_PROFILE`
  won't until your next `kk` switch.
- **One namespace per context per shell:** the overlay reuses the real context
  name, so a shell has a single namespace for a given context at a time.

## Design

See [docs/design.md](docs/design.md) for the design rationale, alternatives
considered and open questions.
