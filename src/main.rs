mod abbrev;
mod catalogue;
mod commands;
mod completion;
mod import;
mod kubeconfig;
mod namespaces;
mod paths;
mod picker;
mod shell;

use std::process::ExitCode;

use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::ArgValueCandidates;

use commands::{App, Cancelled, Silent};
use shell::ShellKind;

/// Per-shell Kubernetes context and namespace switching.
///
/// Each shell gets its own small overlay in front of ~/.kube/config, so windows can
/// point at the same cluster in different namespaces without affecting each other.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Switch this shell to a profile, picking one if no name is given
    #[command(alias = "p")]
    Profile(ProfileArgs),
    /// Switch this shell to a context, picking one if none is given
    #[command(visible_alias = "ctx")]
    Context {
        #[arg(add = ArgValueCandidates::new(completion::contexts))]
        context: Option<String>,
    },
    /// Switch this shell to a namespace in the current context, picking one if none is given
    #[command(visible_alias = "ns")]
    Namespace {
        #[arg(add = ArgValueCandidates::new(completion::namespaces))]
        namespace: Option<String>,
        /// Re-fetch the namespace list instead of using the cache
        #[arg(long)]
        refresh: bool,
    },
    /// Remove overlays left behind by shells that have exited
    Clean {
        /// Also remove overlays holding entries kk didn't write (e.g. clusters added by other tools)
        #[arg(long)]
        force: bool,
    },
    /// Move entries other tools added to this shell's overlay into the main kubeconfig
    Import {
        /// Show what would be imported without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Replace differing entries in the main kubeconfig without asking
        #[arg(long)]
        overwrite: bool,
    },
    /// Return this shell to the plain main kubeconfig
    Exit {
        /// Exit even if the overlay holds entries kk didn't write, discarding them
        #[arg(long)]
        force: bool,
    },
    /// Print the shell integration; add `eval "$(command kk init zsh)"` to your rc file
    Init {
        shell: ShellKind,
        /// Also define the kp, kctx and kns aliases
        #[arg(long)]
        aliases: bool,
    },
    /// Update the cached namespace list for a context (used for background refreshes)
    #[command(hide = true)]
    RefreshNamespaces { context: String },
}

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
struct ProfileArgs {
    #[command(subcommand)]
    action: Option<ProfileAction>,
    /// Profile name, `<context>/<namespace>`, or `-` for this shell's previous profile
    #[arg(add = ArgValueCandidates::new(completion::profiles))]
    profile: Option<String>,
}

#[derive(Subcommand)]
enum ProfileAction {
    /// Name the current shell's profile
    Name { name: String },
    /// List profiles, most relevant first
    #[command(visible_alias = "ls")]
    List,
    /// Remove a profile from the catalogue
    Rm {
        #[arg(add = ArgValueCandidates::new(completion::profiles))]
        name: String,
    },
    /// Print the current profile's name (or <context>/<namespace>), e.g. for a prompt
    Current {
        /// Abbreviate the context and namespace, keeping them unique among known ones
        #[arg(short, long)]
        short: bool,
    },
}

fn main() -> ExitCode {
    clap_complete::CompleteEnv::with_factory(Cli::command)
        .var("KK_COMPLETE")
        .complete();

    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.is::<Cancelled>() => ExitCode::from(130),
        Err(e) if e.is::<Silent>() => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("kk: error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let app = App::new()?;
    match cli.command {
        Command::Profile(args) => match args.action {
            Some(ProfileAction::Name { name }) => app.profile_name(&name),
            Some(ProfileAction::List) => app.profile_list(),
            Some(ProfileAction::Rm { name }) => app.profile_rm(&name),
            Some(ProfileAction::Current { short }) => app.profile_current(short),
            None => app.profile(args.profile),
        },
        Command::Context { context } => app.context(context),
        Command::Namespace { namespace, refresh } => app.namespace(namespace, refresh),
        Command::Clean { force } => app.clean(force),
        Command::Exit { force } => app.exit(force),
        Command::Import { dry_run, overwrite } => app.import(dry_run, overwrite),
        Command::Init { shell, aliases } => app.init(shell, aliases),
        Command::RefreshNamespaces { context } => app.refresh_namespaces(&context),
    }
}
