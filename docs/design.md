# kk — design

A command line tool for managing Kubernetes configuration with per-shell isolation.
Several terminal windows can target the same cluster in different namespaces
without affecting each other, and without keeping copies of the main kubeconfig.

## Problems to solve

1. **Global switching.** `kubectx`/`kubens` rewrite `current-context` in
   `~/.kube/config`, so switching in one window switches every window.
2. **Managing copies.** The current workaround is several copies of the kubeconfig,
   plus a script that sets `KUBECONFIG`. The copies drift and are a hassle to maintain.
3. **Writes landing in the wrong file.** Tools that update the kubeconfig, such as
   credential refreshes or `aws eks update-kubeconfig`, end up writing to whichever
   copy `KUBECONFIG` points at instead of the main config.
4. **Shared namespace in shared files.** Two windows using the same file still share
   its namespace setting.

## Core mechanism: per-shell overlay files

Use a small per-shell overlay file layered in front of the main config instead of
full copies:

```sh
KUBECONFIG=<per-shell overlay>:~/.kube/config
```

The overlay contains only:

- `current-context`
- a single *shadow* context stanza: the same name, cluster and user as in the main
  config, plus the chosen `namespace` and an optional user override

```yaml
apiVersion: v1
kind: Config
current-context: prod-cluster
contexts:
- name: prod-cluster
  context:
    cluster: prod-cluster
    user: prod-user
    namespace: team-a
```

This works because of kubeconfig merge and write rules:

- **On read, the first file to define a name wins**, so the overlay's
  `current-context` and context stanza take effect.
- **On write, a modified value goes to the file that defines its stanza.** Namespace
  changes therefore go to the overlay, while token or cert refreshes and edits to
  existing clusters and users go to `~/.kube/config`, because those stanzas exist
  only there.
- **A newly created entry goes to the first file**, which is the overlay. See the
  caveats below.

`~/.kube/config` remains the single source of truth. Overlays are tiny and disposable.

### Per-shell state

- Overlays live in `${XDG_RUNTIME_DIR:-/tmp}/kk/` with mode `0700`.
- Each overlay is keyed on the shell's PID (`overlay.<pid>`).
- `kk clean` deletes overlays whose PID is no longer alive.
- The previous profile is tracked per shell, not globally, so that `-` (go back)
  works independently in each window.
- A per-shell state file (`state.<pid>.json`) records:
  - the previous profile
  - the env vars set from the profile
  - the name of the context entry kk wrote into the overlay

  It's kept as long as the overlay is.

## Architecture: binary + thin shell function

A child process cannot change its parent shell's environment. So:

- **The `kk` binary** does all the work: resolves contexts, writes overlays,
  maintains the catalogue, runs pickers.
- **A `kk` shell function**, installed with `eval "$(command kk init zsh)"` (or
  `bash`), wraps the binary. On each call it:
  - creates a temp file and runs the binary with `KK_SHELL_PID=$$` and
    `KK_EVAL_FILE=<temp file>`
  - sources the file afterwards if the binary wrote shell code to it
    (`export KUBECONFIG=…`, `export AWS_PROFILE=…`, `unset …`)
  - deletes the file

  Stdout and stderr stay untouched, so `kk profile list` and the pickers behave
  normally and the function doesn't need to know which subcommands change the
  environment. Environment-changing subcommands run without the function fail with
  setup instructions. `kubeswitch` uses a similar binary-plus-function pattern.

This avoids spawning a subshell (as `kubie` does), so zsh history sharing and similar
features keep working.

## Global state: a catalogue of profiles

A **profile** is a combination of context, namespace, optional user override and
optional extra environment variables. All the combinations you have used live in a
global catalogue, and any shell can select one into its own overlay.

`~/.config/kk/profiles.yaml`:

```yaml
profiles:
  - name: prod-team-a            # set via `kk profile name`; omitted for auto-recorded entries
    context: prod-cluster        # context name from ~/.kube/config
    namespace: team-a
    env:                         # optional extras exported into the shell
      AWS_PROFILE: prod
    last_used: 2026-10-28T14:55:00Z
    use_count: 42
  - context: staging-cluster     # auto-recorded, unnamed
    namespace: team-b
    user: staging-readonly       # optional override of the context's user
    last_used: 2026-10-27T09:12:00Z
    use_count: 3
```

- **Identity:** a profile is identified by its `(context, namespace, user)`
  combination. At most one catalogue entry exists per combination.
- **Auto-recording:** whenever a shell switches to a combination with
  `kk context` or `kk namespace`, it is upserted into the catalogue.
- **Usage tracking:** every switch to a profile, by any `kk profile`,
  `kk context` or `kk namespace` command, sets `last_used` to now and increments
  `use_count`. Re-selecting the profile the shell is already on doesn't count. See
  [Ordering](#ordering-recency-then-frequency).
- **Locking:** take a file lock on write, since many shells touch the file.
- **Non-kubeconfig settings** such as `AWS_PROFILE` or `HELM_NAMESPACE` live in the
  catalogue as `env`, not in kubeconfig.

### Profile names

- **Default name:** an auto-recorded profile has no stored `name`. It is displayed
  and addressed as `<context>/<namespace>`, e.g. `prod-cluster/team-a`, with
  `@<user>` appended when the profile overrides the context's user. Because the
  default is derived, renaming a context in the main config never leaves a stale
  default name behind.
- **Naming:** `kk profile name <new-name>` gives the current shell's profile a
  name. This sets `name` on the catalogue entry for the current combination. After
  that the profile is shown and addressed by that name, but `kk profile
  prod-cluster/team-a` still resolves to it.
- **Constraints:**
  - Names must be unique.
  - Names may not contain `/`, so they can never collide with a derived default name.
  - Names may not be a reserved subcommand word (`name`, `list`, `ls`, `rm`, `current`), start
    with `-`, or contain whitespace.
- Context names can be long, for example EKS ARNs like
  `arn:aws:eks:eu-west-1:123456789012:cluster/prod`. Derived names for those are
  unwieldy, which is the main reason to name a profile.

### Ordering: recency, then frequency

Picker and `kk profile list` order combines recency and frequency. Recently used
profiles come first. As a profile's last use gets older, frequency of use
increasingly decides its position.

1. **Recent tier:** profiles used within the last hour come first, ordered purely
   by `last_used`, newest first. This covers jumping back to something you just
   used in another window.
2. **Everything else:** ordered by a frecency score, highest first:

   ```
   score = use_count × weight(age of last_used)
   ```

   | Age of `last_used` | Weight |
   |---|---|
   | < 1 day | 1.0 |
   | < 1 week | 0.5 |
   | < 1 month | 0.25 |
   | older | 0.1 |

   Between two profiles of similar age, the more used one wins. A heavily used
   profile from last week still outranks one used once yesterday. A profile not
   touched for months sinks unless it was used a lot.

   Ties are broken by `last_used`, newest first.

**Aging counts:** without a limit, old habits would dominate forever. When the
sum of all `use_count`s exceeds 1000:

- every count is multiplied by 0.9, rounding down
- unnamed profiles whose count reaches 0 are removed from the catalogue
- named profiles are never removed; their count stays at least 1

This also provides automatic pruning of auto-recorded combinations. zoxide uses
the same scheme.

**Picker integration:**

- `kk` passes candidates to `fzf` already sorted.
- With an empty query, the order is shown as-is.
- When typing, fzf ranks by match quality, and `kk` passes `--tiebreak=index` so
  that equally good matches keep the frecency order.
- The built-in fallback picker should behave the same way.
- The current shell's profile is shown with a marker, so it is easy to tell apart
  from the other entries.

The recent-tier window, bucket weights and aging threshold are constants
initially. They could become settings later.

## Commands

All functionality is exposed as subcommands of `kk`.

| Command | Behaviour |
|---|---|
| `kk profile` | fzf picker over profiles, recent first then by frequency (see Ordering); switches this shell to the chosen one |
| `kk profile <name>` | switch to a profile by name (or derived `<context>/<namespace>` name) |
| `kk profile -` | return to this shell's previous profile |
| `kk profile name <new-name>` | name the current shell's profile |
| `kk profile list` | list the catalogue |
| `kk profile current [--short]` | print the current profile's name, or `<context>/<namespace>` if unnamed; for prompts (see below). `--short` abbreviates an unnamed profile's context and namespace |
| `kk profile rm <name>` | remove a profile from the catalogue |
| `kk context [context]` | switch context in this shell (fzf if no argument), keeping the namespace from the most recent profile for that context, else `default`; records the combination |
| `kk namespace [namespace]` | switch namespace within the current context (fzf if no argument); records the combination |
| `kk import [--dry-run] [--overwrite]` | move entries other tools added to this shell's overlay into the main config (see [Overlay migration](#overlay-migration)) |
| `kk clean [--force]` | remove overlays for dead PIDs, importing their foreign entries first; keeps overlays that still hold some (unless `--force`) |
| `kk exit [--force]` | delete this shell's overlay, unset `KUBECONFIG` and any profile `env` vars; back to the plain main config. Imports first; refuses if foreign entries remain (unless `--force`) |
| `kk init <shell>` | print the shell function (and optional aliases) to `eval` from the rc file |

Switching to a profile writes or rewrites this shell's overlay and exports
`KUBECONFIG` and the profile's `env`. It also unsets any `env` vars set by the
previous profile that the new one doesn't define.

Short aliases are left to the shell, for example:

```sh
alias kp='kk profile'
alias kctx='kk context'
alias kns='kk namespace'
```

`kk init` could print these with a flag such as `--aliases`.

- **Pickers:** fzf (or the built-in fallback, see Implementation) for contexts,
  namespaces and profiles.
- **Namespace cache:** cache the namespace list per cluster so pickers are instant.
- **Prompt:** `kk profile current` is designed to run on every prompt.
  - It finds the shell's overlay from the first entry of `KUBECONFIG`, so it works
    as `command kk profile current` without the shell function.
  - It reflects namespace changes made with `kubectl` directly.
  - Without an active overlay, it falls back to the main config's current context.
    With no context at all, it prints nothing and exits 1.

  - `--short` abbreviates long derived names for prompts, e.g.
    `public-local-se-sto-management/evroc-operator-system` → `p-l-s-s-m/e-o-s`.
    - Names are split into segments at `-`, `_`, `.`, `:` and `/`.
    - Left to right, each segment is cut to the shortest prefix that keeps the name
      unique among its peers (powerlevel10k's `truncate_to_unique`, applied to name
      segments).
    - The peers of the context are the main config's contexts. The peers of the
      namespace are the namespaces that catalogue profiles use with that context;
      no cluster query is made.
    - An abbreviation matches a peer when the separators are identical and each
      segment is a prefix. Segments left at full length must match exactly, so the
      result is always unique.
    - Named profiles are never abbreviated.

  `kube-ps1` reads the merged config, so it should show context/namespace
  unchanged. This is unverified. `kk` could also export `KK_PROFILE` for prompts
  that want to show the profile name.

## Caveats

- **New entries land in the overlay.** Commands that *add* clusters or users, such as
  `aws eks update-kubeconfig` or `gcloud container clusters get-credentials`, create
  them in the first file. They usually also set `current-context` there, switching
  the shell to the new context. See [Overlay migration](#overlay-migration).
- **Exec/OIDC auth:** with the overlay model, refreshed credentials go to the main
  file, unlike the copy-based approach.

## Overlay migration

kk moves entries other tools write into an overlay into the main kubeconfig,
where they would have gone without kk.

- **Telling kk's entry apart:** the shell's state file records the name of the
  context entry kk wrote (`shadow`). Everything else in the overlay is foreign. kk
  doesn't infer its own entry from `current-context`, because tools change that.
  For state written before this existed, kk falls back to `current-context`.
- **When it runs:**
  - **Automatically,** at the start of `kk profile`, `kk context`, `kk namespace`
    and `kk exit` in that shell, unless `KK_AUTO_IMPORT=0`.
  - **Explicitly,** with `kk import [--dry-run] [--overwrite]`.
  - **In `kk clean`,** for dead shells' overlays, before deleting them. This is
    also skipped when `KK_AUTO_IMPORT=0`.
  - **Never** in the opportunistic cleanup during a switch, since that may need
    to ask about conflicts.
- **What moves:**
  - clusters, users, and contexts other than kk's. Other top-level keys are left
    in the overlay with a warning.
  - **New names** are appended to the main config's lists.
  - **Identical entries** already in the main config are just removed from the
    overlay.
  - **A different entry with the same name is a conflict:**
    - **`--overwrite`:** replaces the main config's version.
    - **With a terminal:** kk shows the differing field paths and asks
      (default no). Values are shown only for context fields and `*.server`, never
      for user credentials or certificate data.
    - **Otherwise:** the entry stays in the overlay, with a warning.
- **Following a switch:** if the overlay's `current-context` is no longer kk's
  context, and that context is in the main config after the import, kk switches
  the shell to it with a normal kk overlay. The previous profile is what kk had
  before. This also covers `kubectl config use-context` in a kk shell.
- **Writing the main config:**
  - under kubectl's lock file (`<file>.lock`, created exclusively, as client-go
    does), waiting up to 3 seconds
  - after copying the current file to `<file>.kk-backup`, a single rolling backup
  - replaced atomically, through any symlink, keeping its permissions
  - unknown fields and order are preserved; YAML comments are not
- **Guards:**
  - `kk exit` and `kk clean` refuse to delete an overlay that still holds foreign
    entries, unless `--force` is passed. That covers declined conflicts, auto-import
    turned off, and unmergeable keys.
  - Unparseable overlays are kept.
  - A switch carries foreign entries over into the rewritten overlay, with a
    warning.
- **Prompt marker:** `kk profile current` appends `!` when the overlay has foreign
  entries or a `current-context` other than kk's.

## Implementation: Rust

- **CLI:** `clap` (derive) for subcommands.
- **Kubeconfig handling:** `kk` reads only the main config and writes only its own
  overlays, so it doesn't reimplement the full merge logic.
  - **Reading the main config:** plain `serde` structs (`serde_norway`) parse just
    its `contexts` and `current-context`.
  - **Writing overlays:** they are handled as untyped YAML. Anything another tool
    added to an overlay (clusters, users, extra contexts) is carried over with a
    warning rather than dropped.
  - **Writing to the main config:** if `kk` ever does this, for example when
    migrating stray entries back, unknown fields must round-trip.
- **Namespace listing:** shells out to
  `kubectl --context <ctx> get namespaces` against the main config, so exec/OIDC
  auth plugins work unchanged.
  - Results are cached per context in `~/.cache/kk/namespaces/`.
  - A cached list is always used immediately. If it is more than 5 minutes old, a
    detached `kk refresh-namespaces` updates it for next time.
  - `--refresh` forces a fetch.
  - If listing fails, the picker still offers namespaces already used with that
    context.
  - A `kube` client could replace kubectl later.
- **Picker:** external `fzf`, with a built-in fallback:
  - **Default:** if `fzf` is on `PATH`, `kk` pipes the candidates to it. The user's
    `FZF_DEFAULT_OPTS` (colours, layout, keybindings) applies as usual.
  - **Fallback:** if `fzf` isn't installed, `kk` uses a lightweight built-in fuzzy
    select from `dialoguer` or `inquire`, so `kk` works with no extra dependencies.
  - **Override:** `KK_PICKER` sets the picker command, e.g. `KK_PICKER=sk` or
    `KK_PICKER="fzf --height 40%"`. `KK_PICKER=builtin` forces the fallback. Any
    command works as long as it reads candidates on stdin and prints the selection
    on stdout.
  - **TTY:** the `kk` shell function `eval`s the binary's stdout, so the picker must
    draw on `/dev/tty`, not stdout. `fzf` and the built-in pickers already do this.
  - **Rejected: embedding `skim`.** It's fzf-like and needs no runtime dependency,
    but:
    - it noticeably increases binary size and compile time
    - its maintenance has been uneven
    - the library is Unix-only
    - it ignores the user's existing fzf configuration
- **Locking:** an advisory lock (`std::fs::File::lock`) on `profiles.lock`
  around catalogue read-modify-write. The file itself is replaced atomically.
- **Paths:** XDG directories via `dirs`. Without `XDG_RUNTIME_DIR` (e.g. macOS),
  overlays go in `$TMPDIR/kk-<uid>/`. `KK_MAIN_KUBECONFIG`, `KK_CONFIG_DIR`,
  `KK_RUNTIME_DIR` and `KK_CACHE_DIR` override the locations, mainly for testing.
- **Subcommand aliases:** `kk p` (hidden), `kk ctx`, `kk ns` and `kk profile ls`, in
  addition to the shell aliases `kk init --aliases` defines.
- **Shells:** bash and zsh at minimum; fish to be decided. Only the `kk init`
  snippet differs per shell.
- **Tab completion:** uses `clap_complete`'s dynamic completion.
  - `kk init` registers it by evaluating `KK_COMPLETE=<shell> kk`. The shell then
    calls back into the binary on each TAB.
  - `ArgValueCandidates` on the context, namespace and profile arguments supply
    values in the same order as the pickers.
  - Namespace candidates come only from the catalogue and the cache, never a live
    `kubectl` call. A missing or stale cache triggers a background refresh.
  - Registration is for the name `kk`, so it applies to the shell function. zsh
    also completes the `kp`/`kctx`/`kns` aliases, but bash doesn't complete aliases.
  - `kk p` is a hidden alias. If it were visible, the completion engine would offer
    `p` instead of `profile`.

## Open decisions

- **Renamed or removed contexts:** flag the profile as stale rather than deleting it.
  This is the leaning so far.
- **Ordering constants:** are a 1-hour recent window and the weights above right in
  practice? Tune them after real use.
- **Shadow context name:** keep the original context name (simpler, `kube-ps1` shows
  the real name, at most one namespace per context per shell, which fits the workflow)
  or use the profile name? The leaning is to keep the original name.
- **Fallback picker crate:** `dialoguer` vs `inquire`.

## Prior art

- **kubeswitch (Go):** closest in spirit. It uses per-session temp kubeconfigs via
  shell functions and has store integrations (EKS, GKE, AKS, Vault, …). It uses full
  temp copies rather than overlays.
- **kubie (Rust):** per-shell isolation via a subshell, which breaks zsh history sharing.
- **kubeprompt:** per-shell `KUBECONFIG` isolation plus a prompt indicator.
- **kubectx / kubens:** global; this is the problem being solved.
