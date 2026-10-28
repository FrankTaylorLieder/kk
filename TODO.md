# TODO

1. **Tab completion for contexts, profiles and namespaces.** Complete values on the
   command line for `kk context <TAB>`, `kk profile <TAB>` (including `kk profile rm`)
   and `kk namespace <TAB>`, in bash and zsh.
   - Contexts: from the main kubeconfig.
   - Profiles: names and derived `<context>/<namespace>` names from the catalogue,
     in frecency order.
   - Namespaces: from the namespace cache for the current context (never call
     kubectl synchronously while completing), plus namespaces already used with it.
   - Likely approach: `clap_complete`'s dynamic completion
     (`CompleteEnv`/`ArgValueCandidates`), with `kk init` also emitting the
     completion setup. The completion must apply to the `kk` shell function, not
     only the binary.
