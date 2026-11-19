//! Command-line surface, modelled on Docker and Podman.

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "oci-hermit",
    version,
    about = "Run OCI images in a sealed, software-emulated sandbox",
    long_about = "Run OCI images in a sealed, software-emulated sandbox.\n\n\
                  No root, no admin, no hypervisor. No egress; ingress only \
                  through ports you publish on 127.0.0.1."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Pull an image from a registry
    Pull {
        /// Image platform, e.g. linux/amd64 or linux/arm64 (default: the host's)
        #[arg(long)]
        platform: Option<String>,
        /// Folder with the registry's CA certificates (*.crt) and client certificate (*.cert, *.key), instead of
        /// the certs.d folders
        #[arg(long, value_name = "DIR")]
        cert_dir: Option<std::path::PathBuf>,
        /// Image reference, e.g. debian:bookworm-slim or ghcr.io/org/app:1.2
        image: String,
    },
    /// Log in to a registry, storing the login where Podman keeps it
    Login(LoginArgs),
    /// Remove a registry login
    Logout(LogoutArgs),
    /// Load an image from a docker-archive or OCI layout tarball
    Load {
        /// Path to the tarball or OCI layout directory
        #[arg(short, long)]
        input: String,
    },
    /// List images
    Images(ImagesArgs),
    /// Remove one or more images
    Rmi(RmiArgs),
    /// Show details of images as JSON
    Inspect(InspectArgs),
    /// Give an image another name
    Tag(TagArgs),
    /// Save images to an archive that `load` (and Docker and Podman) read
    Save(SaveArgs),
    /// Manage images
    #[command(subcommand)]
    Image(ImageCommand),
    /// Look at image manifests in a registry, without pulling
    #[command(subcommand)]
    Manifest(ManifestCommand),
    /// Show what oci-hermit uses and where it keeps things (as `machine info`)
    Info,
    /// Manage oci-hermit's data
    #[command(subcommand)]
    System(SystemCommand),
    /// Run a command in a new container
    Run(RunArgs),
    /// Run a command in a running container
    Exec(ExecArgs),
    /// Show a container's logs
    Logs {
        /// Keep showing new output until the container stops
        #[arg(short, long)]
        follow: bool,
        container: String,
    },
    /// List containers
    Ps {
        /// Include stopped containers
        #[arg(short, long)]
        all: bool,
        /// Only show container IDs
        #[arg(short, long)]
        quiet: bool,
        /// Show whole IDs and commands
        #[arg(long)]
        no_trunc: bool,
    },
    /// Stop one or more running containers
    Stop {
        /// Seconds to wait for it to stop before killing it
        #[arg(short = 't', long = "time")]
        time: Option<u64>,
        /// Signal to send first, instead of the image's stop signal (SIGTERM by default)
        #[arg(short, long)]
        signal: Option<String>,
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Send a signal to one or more running containers
    Kill {
        /// Signal to send, e.g. TERM, SIGHUP or 9
        #[arg(short, long, default_value = "KILL")]
        signal: String,
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Remove one or more containers
    Rm {
        /// Stop running containers first
        #[arg(short, long)]
        force: bool,
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Copy files between a container and the host
    Cp { source: String, destination: String },
    /// Manage pods (containers that share localhost)
    #[command(subcommand)]
    Pod(PodCommand),
    /// Run a Compose project (supported subset)
    #[command(subcommand)]
    Compose(ComposeCommand),
    /// Show details of the emulated sandbox machine
    #[command(subcommand)]
    Machine(MachineCommand),
    /// Print version information
    Version,
    /// Run one container's sandbox (started by `run`, not by people)
    #[command(name = "__monitor", hide = true)]
    Monitor { data_dir: std::path::PathBuf, id: String },
}

impl Command {
    /// The command's name as typed on the command line.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Pull { .. } => "pull",
            Command::Login(_) => "login",
            Command::Logout(_) => "logout",
            Command::Load { .. } => "load",
            Command::Images(_) => "images",
            Command::Rmi(_) => "rmi",
            Command::Inspect(_) => "inspect",
            Command::Tag(_) => "tag",
            Command::Save(_) => "save",
            Command::Image(_) => "image",
            Command::Manifest(_) => "manifest",
            Command::Info => "info",
            Command::System(_) => "system",
            Command::Run(_) => "run",
            Command::Exec(_) => "exec",
            Command::Logs { .. } => "logs",
            Command::Ps { .. } => "ps",
            Command::Stop { .. } => "stop",
            Command::Kill { .. } => "kill",
            Command::Rm { .. } => "rm",
            Command::Cp { .. } => "cp",
            Command::Pod(_) => "pod",
            Command::Compose(_) => "compose",
            Command::Machine(_) => "machine",
            Command::Version => "version",
            Command::Monitor { .. } => "__monitor",
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum ManifestCommand {
    /// Show an image's index or manifest as the registry serves it (platforms, digests, sizes)
    Inspect(ManifestInspectArgs),
}

#[derive(Debug, Args)]
pub struct ManifestInspectArgs {
    /// Resolve each platform's manifest too, as `docker manifest inspect --verbose` does
    #[arg(short, long)]
    pub verbose: bool,
    /// Folder with the registry's CA certificates (*.crt) and client certificate (*.cert, *.key), instead of the
    /// certs.d folders
    #[arg(long, value_name = "DIR")]
    pub cert_dir: Option<std::path::PathBuf>,
    /// Image reference, e.g. debian:bookworm-slim or ghcr.io/org/app:1.2
    pub image: String,
}

#[derive(Debug, Args)]
pub struct LoginArgs {
    /// User name (asked for if not given)
    #[arg(short, long)]
    pub username: Option<String>,
    /// Password; visible to other users in the process list, so prefer --password-stdin
    #[arg(short, long)]
    pub password: Option<String>,
    /// Read the password from stdin
    #[arg(long)]
    pub password_stdin: bool,
    /// Auth file to write (default: Podman's)
    #[arg(long)]
    pub authfile: Option<String>,
    /// Print the user name logged in to the registry, and nothing else
    #[arg(long)]
    pub get_login: bool,
    /// Folder with the registry's CA certificates (*.crt) and client certificate (*.cert, *.key), instead of the
    /// certs.d folders
    #[arg(long, value_name = "DIR")]
    pub cert_dir: Option<std::path::PathBuf>,
    /// Registry host, e.g. ghcr.io or registry.example.com:5000 (default: docker.io)
    pub registry: Option<String>,
}

#[derive(Debug, Args)]
pub struct LogoutArgs {
    /// Remove the logins for every registry in the auth file
    #[arg(short, long)]
    pub all: bool,
    /// Auth file to change (default: Podman's)
    #[arg(long)]
    pub authfile: Option<String>,
    /// Registry host (default: docker.io)
    pub registry: Option<String>,
}

#[derive(Debug, Args)]
pub struct ImagesArgs {
    /// Only images from this repository, or with this name
    pub reference: Option<String>,
    /// Only show image IDs
    #[arg(short, long)]
    pub quiet: bool,
    /// Show each image's manifest digest
    #[arg(long)]
    pub digests: bool,
    /// Show full image IDs
    #[arg(long)]
    pub no_trunc: bool,
}

#[derive(Debug, Args)]
pub struct RmiArgs {
    /// Remove every name of an image given by ID
    #[arg(short, long)]
    pub force: bool,
    /// Names or IDs
    #[arg(required = true)]
    pub images: Vec<String>,
}

#[derive(Debug, Args)]
pub struct InspectArgs {
    /// Names or IDs
    #[arg(required = true)]
    pub images: Vec<String>,
}

#[derive(Debug, Args)]
pub struct TagArgs {
    /// Name or ID of a stored image
    pub source: String,
    /// The new name
    pub target: String,
}

#[derive(Debug, Args)]
pub struct SaveArgs {
    /// Write to this file
    #[arg(short, long, required = true)]
    pub output: String,
    /// Only this platform, e.g. linux/arm64 (default: every platform stored)
    #[arg(long)]
    pub platform: Option<String>,
    /// Names or IDs
    #[arg(required = true)]
    pub images: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum ImageCommand {
    /// List images
    #[command(alias = "list")]
    Ls(ImagesArgs),
    /// Remove one or more images
    #[command(alias = "remove")]
    Rm(RmiArgs),
    /// Show details of images as JSON
    Inspect(InspectArgs),
    /// Remove images loaded without a name, or every image with -a
    Prune {
        /// Remove every image, not only unnamed ones
        #[arg(short, long)]
        all: bool,
        /// Do not ask for confirmation
        #[arg(short, long)]
        force: bool,
    },
    /// Show the steps that built an image
    History {
        /// Do not shorten IDs and commands
        #[arg(long)]
        no_trunc: bool,
        /// Only show image IDs
        #[arg(short, long)]
        quiet: bool,
        image: String,
    },
    /// Exit 0 if an image is stored, 1 if not
    Exists { image: String },
    /// Give an image another name
    Tag(TagArgs),
    /// Save images to an archive
    Save(SaveArgs),
}

#[derive(Debug, Subcommand)]
pub enum SystemCommand {
    /// Show how much space images use
    Df,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Keep STDIN open
    #[arg(short, long)]
    pub interactive: bool,
    /// Allocate a pseudo-TTY
    #[arg(short, long)]
    pub tty: bool,
    /// Run in the background
    #[arg(short, long)]
    pub detach: bool,
    /// Remove the container when it exits
    #[arg(long)]
    pub rm: bool,
    /// Container name
    #[arg(long)]
    pub name: Option<String>,
    /// Publish a port: [127.0.0.1:]HOST:CONTAINER (always bound to 127.0.0.1)
    #[arg(short, long = "publish")]
    pub publish: Vec<String>,
    /// Bind-mount a host folder: SRC:DST[:ro|:rw] (read-only by default)
    #[arg(short, long = "volume")]
    pub volume: Vec<String>,
    /// Join an existing pod
    #[arg(long)]
    pub pod: Option<String>,
    /// Image platform, e.g. linux/amd64 or linux/arm64
    #[arg(long)]
    pub platform: Option<String>,
    /// Number of emulated CPUs
    #[arg(long)]
    pub cpus: Option<u32>,
    /// Memory for the sandbox, e.g. 1g or 512m
    #[arg(short, long)]
    pub memory: Option<String>,
    /// Run as this user instead of the image's: name|uid[:group|gid]
    #[arg(short, long)]
    pub user: Option<String>,
    /// Image to run: a reference (pulled if it is not in the store yet), or an OCI layout directory or .tar
    pub image: String,
    /// Command and arguments (defaults to the image's entrypoint)
    #[arg(trailing_var_arg = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ExecArgs {
    /// Keep STDIN open
    #[arg(short, long)]
    pub interactive: bool,
    /// Allocate a pseudo-TTY
    #[arg(short, long)]
    pub tty: bool,
    /// Run as this user instead of the container's: name|uid[:group|gid]
    #[arg(short, long)]
    pub user: Option<String>,
    /// Set an environment variable: NAME=value
    #[arg(short, long = "env")]
    pub env: Vec<String>,
    /// Working directory inside the container
    #[arg(short, long)]
    pub workdir: Option<String>,
    /// Container name or ID
    pub container: String,
    /// Command and arguments
    #[arg(trailing_var_arg = true, required = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum PodCommand {
    /// Create a pod
    Create {
        #[arg(long)]
        name: String,
        /// Publish a port for the whole pod
        #[arg(short, long = "publish")]
        publish: Vec<String>,
    },
    /// Remove a pod
    Rm { pod: String },
    /// List pods
    Ps,
}

#[derive(Debug, Subcommand)]
pub enum ComposeCommand {
    /// Create and start the project
    Up {
        #[arg(short, long)]
        detach: bool,
        /// Compose file
        #[arg(short, long)]
        file: Option<String>,
    },
    /// Stop and remove the project
    Down {
        #[arg(short, long)]
        file: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum MachineCommand {
    /// Show emulator, guest kernel and limits
    Info,
    /// Boot a sandbox VM until its agent is ready, then stop it: a self-check of QEMU, kernel and agent
    Boot(BootArgs),
}

#[derive(Debug, Args)]
pub struct BootArgs {
    /// Guest architecture (default: the host's)
    #[arg(long, value_enum)]
    pub arch: Option<crate::qemu::GuestArch>,
    /// Memory for the sandbox, in MiB
    #[arg(long, default_value_t = 512)]
    pub memory: u32,
    /// Number of emulated CPUs
    #[arg(long, default_value_t = 1)]
    pub cpus: u32,
    /// Seconds to wait for the agent to be ready
    #[arg(long, default_value_t = 120)]
    pub timeout: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn run_parses_docker_style_flags() {
        let cli = Cli::try_parse_from([
            "oci-hermit",
            "run",
            "-it",
            "--rm",
            "-p",
            "8080:80",
            "-v",
            "./data:/data",
            "alpine",
            "sh",
            "-c",
            "echo hi",
        ])
        .unwrap();
        let Command::Run(args) = cli.command else {
            panic!("expected run");
        };
        assert!(args.interactive && args.tty && args.rm);
        assert_eq!(args.publish, ["8080:80"]);
        assert_eq!(args.volume, ["./data:/data"]);
        assert_eq!(args.image, "alpine");
        assert_eq!(args.command, ["sh", "-c", "echo hi"]);
    }

    #[test]
    fn every_command_reports_its_own_name() {
        let cases: &[(&[&str], &str)] = &[
            (&["pull", "alpine"], "pull"),
            (&["load", "-i", "app.tar"], "load"),
            (&["login", "-u", "me", "ghcr.io"], "login"),
            (&["logout", "--all"], "logout"),
            (&["images"], "images"),
            (&["rmi", "alpine"], "rmi"),
            (&["inspect", "web"], "inspect"),
            (&["tag", "alpine", "mine"], "tag"),
            (&["save", "-o", "a.tar", "alpine"], "save"),
            (&["image", "ls"], "image"),
            (&["manifest", "inspect", "alpine"], "manifest"),
            (&["info"], "info"),
            (&["system", "df"], "system"),
            (&["run", "alpine"], "run"),
            (&["exec", "web", "sh"], "exec"),
            (&["logs", "-f", "web"], "logs"),
            (&["ps", "-a"], "ps"),
            (&["stop", "web"], "stop"),
            (&["kill", "web"], "kill"),
            (&["rm", "web"], "rm"),
            (&["cp", "web:/a", "."], "cp"),
            (&["pod", "ps"], "pod"),
            (&["compose", "up", "-d"], "compose"),
            (&["machine", "info"], "machine"),
            (&["version"], "version"),
        ];
        for (args, name) in cases {
            let cli = Cli::try_parse_from(std::iter::once("oci-hermit").chain(args.iter().copied())).unwrap();
            assert_eq!(cli.command.name(), *name, "for {args:?}");
        }
    }

    #[test]
    fn pod_create_parses() {
        let cli = Cli::try_parse_from(["oci-hermit", "pod", "create", "--name", "shop", "-p", "8080:3000"]).unwrap();
        assert_eq!(cli.command.name(), "pod");
    }
}
