# TODO

1. **Capture new contexts added to overlays.** Tools that add clusters, such as
   `aws eks update-kubeconfig` or `gcloud container clusters get-credentials`, write
   their new cluster, user and context entries into the first `KUBECONFIG` file,
   which is the shell's overlay. Today `write_overlay` keeps them on the next switch
   with a warning, but they never reach `~/.kube/config`. So they disappear with
   the overlay on `kk exit` or `kk clean`, and other shells can't use them.
   - Detect entries in an overlay that kk didn't write: clusters, users, and any
     context other than the shadow context.
   - Move them into the main kubeconfig, preserving unknown fields and comments as
     far as possible, and remove them from the overlay.
   - When a name clashes with an existing main-config entry with different
     contents, warn and leave it in the overlay rather than overwriting.
   - Decide when this runs:
     - on every `kk` command, which is cheap
     - in `kk exit` and `kk clean`, before an overlay is deleted
     - with an explicit `kk import` command
     - or some combination of these
   - `kk exit` and `kk clean` already refuse to delete an overlay holding such
     entries (unless `--force` is passed). Once migration exists, they should migrate
     the entries first and then delete the overlay.
   - Optionally offer to switch to the newly added context.
