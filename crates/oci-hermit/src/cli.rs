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
    Pull { image: String },
    /// Load an image from a docker-archive or OCI layout tarball
    Load {
        /// Path to the tarball or OCI layout directory
        #[arg(short, long)]
        input: String,
    },
    /// List images
    Images,
    /// Remove one or more images
    Rmi {
        #[arg(required = true)]
        images: Vec<String>,
    },
    /// Show details of an image, container or pod
    Inspect { target: String },
    /// Run a command in a new container
    Run(RunArgs),
    /// Run a command in a running container
    Exec(ExecArgs),
    /// Show a container's logs
    Logs {
        container: String,
        #[arg(short, long)]
        follow: bool,
    },
    /// List containers
    Ps {
        /// Include stopped containers
        #[arg(short, long)]
        all: bool,
    },
    /// Stop one or more containers
    Stop {
        #[arg(required = true)]
        containers: Vec<String>,
    },
    /// Remove one or more containers
    Rm {
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
}

impl Command {
    /// The command's name as typed on the command line.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Pull { .. } => "pull",
            Command::Load { .. } => "load",
            Command::Images => "images",
            Command::Rmi { .. } => "rmi",
            Command::Inspect { .. } => "inspect",
            Command::Run(_) => "run",
            Command::Exec(_) => "exec",
            Command::Logs { .. } => "logs",
            Command::Ps { .. } => "ps",
            Command::Stop { .. } => "stop",
            Command::Rm { .. } => "rm",
            Command::Cp { .. } => "cp",
            Command::Pod(_) => "pod",
            Command::Compose(_) => "compose",
            Command::Machine(_) => "machine",
            Command::Version => "version",
        }
    }
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
    /// Image to run
    pub image: String,
    /// Command and arguments (defaults to the image's entrypoint)
    #[arg(trailing_var_arg = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct ExecArgs {
    #[arg(short, long)]
    pub interactive: bool,
    #[arg(short, long)]
    pub tty: bool,
    pub container: String,
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
            (&["images"], "images"),
            (&["rmi", "alpine"], "rmi"),
            (&["inspect", "web"], "inspect"),
            (&["run", "alpine"], "run"),
            (&["exec", "web", "sh"], "exec"),
            (&["logs", "-f", "web"], "logs"),
            (&["ps", "-a"], "ps"),
            (&["stop", "web"], "stop"),
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
