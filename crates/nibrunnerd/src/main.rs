#![cfg_attr(test, allow(clippy::unwrap_used, clippy::panic, clippy::expect_used))]

use nibrunnerd::config::HostConfig;
use nibrunnerd::controllers::lifecycle_controller::LifecycleController;
use nibrunnerd::install::Origin;
use nibrunnerd::{install, run, start};

/// The three things this binary is ever asked to do. `install` lays a host out and `start` asks
/// systemd for what it laid; everything else this daemon does, it does by serving — and none of
/// them reads anything the others do not.
#[derive(Debug)]
enum Command {
    Serve,
    Install {
        force: bool,
        release: Option<std::path::PathBuf>,
    },
    Start {
        force: bool,
    },
}

const USAGE: &str = "\
nibrunnerd — one binary that turns a Linux machine into an app host.

    nibrunnerd                 serve this host, reading /etc/nibrunner/config.toml
    nibrunnerd install         lay this host out from that file — guest image, kernel settings,
                               ZeroFS, units — and start nothing
    nibrunnerd start           lay it out again, then start what that file names under systemd,
                               restarting whatever read something that changed. Run it after
                               every edit.

    --force                    replace files these commands did not write; both take it
    install --from <dir>       take the guest image from a release unpacked here

`--from` is what `deploy/install.sh` passes: it downloads a release and names the directory,
and everything about where the files in it go is read from the configuration rather than
told twice. The digests the release publishes are checked against either way.

NIBRUNNER_CONFIG names another configuration file. NIBRUNNER_LOG is a tracing filter.
";

impl Command {
    fn flags(
        command: &str,
        arguments: &[String],
        takes_release: bool,
    ) -> Result<(bool, Option<std::path::PathBuf>), String> {
        let mut force = false;
        let mut release = None;
        let mut rest = arguments.iter();
        while let Some(argument) = rest.next() {
            match argument.as_str() {
                "--force" => force = true,
                "--from" if takes_release => {
                    release = Some(rest.next().map(std::path::PathBuf::from).ok_or_else(|| {
                        format!(
                            "nibrunnerd {command}: --from takes the directory a release was unpacked into\n\n{USAGE}"
                        )
                    })?);
                }
                unknown => {
                    return Err(format!(
                        "nibrunnerd {command}: {unknown} is not an argument it takes\n\n{USAGE}"
                    ))
                }
            }
        }
        Ok((force, release))
    }

    fn parse(arguments: impl Iterator<Item = String>) -> Result<Self, String> {
        let arguments: Vec<String> = arguments.collect();
        match arguments.split_first() {
            None => Ok(Self::Serve),
            Some((first, rest)) if first == "install" => {
                let (force, release) = Self::flags("install", rest, true)?;
                Ok(Self::Install { force, release })
            }
            Some((first, rest)) if first == "start" => {
                let (force, _) = Self::flags("start", rest, false)?;
                Ok(Self::Start { force })
            }
            Some((first, _)) if first == "--help" || first == "-h" || first == "help" => {
                Err(USAGE.to_string())
            }
            Some((first, _)) => Err(format!("nibrunnerd: {first} is not a command it has\n\n{USAGE}")),
        }
    }
}

fn main() -> std::process::ExitCode {
    nibrunnerd::install_crypto_provider();

    let command = match Command::parse(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(said) => {
            eprintln!("{said}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // An installer talks to whoever ran it, and a daemon talks to the log store. Structured JSON
    // on stderr is right for one and unreadable for the other.
    if matches!(command, Command::Serve) {
        install_logger();
    }

    let (config, origin) = match configuration(&command) {
        Ok(config) => config,
        Err(code) => return code,
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("the runtime could not be started: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };

    match command {
        Command::Serve => runtime.block_on(serve(config)),
        Command::Install { force, release } => runtime.block_on(lay_out(config, force, release, origin)),
        Command::Start { force } => runtime.block_on(bring_up(config, force)),
    }
}

async fn lay_out(
    config: HostConfig,
    force: bool,
    release: Option<std::path::PathBuf>,
    origin: Origin,
) -> std::process::ExitCode {
    let config_file = HostConfig::configured_file();
    match install::run(&config, &config_file, force, release.as_deref()).await {
        Ok(laid) => {
            print_laid(&laid);
            print!("{}", install::next_steps(&config, &config_file, origin));
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{}", error.message());
            std::process::ExitCode::FAILURE
        }
    }
}

/// The layout again, so an edited `config.toml` is what comes up, then systemd.
async fn bring_up(config: HostConfig, force: bool) -> std::process::ExitCode {
    let config_file = HostConfig::configured_file();
    let laid = match install::run(&config, &config_file, force, None).await {
        Ok(laid) => laid,
        Err(error) => {
            eprintln!("{}", error.message());
            return std::process::ExitCode::FAILURE;
        }
    };
    print_laid(&laid);
    let report = match start::run(&config, &install::environment_file(&config_file), &laid) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("{error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    println!();
    for (unit, outcome) in &report.units {
        println!("  {unit:<32} {}", outcome.said());
    }
    if report.all_up() {
        println!(
            "\nUp. It serves what {} says; `journalctl -u nibrunnerd -f` follows it.",
            config.desired_state_file.display()
        );
        return std::process::ExitCode::SUCCESS;
    }
    eprintln!("\nNot up. What went wrong: {}", report.journal());
    std::process::ExitCode::FAILURE
}

/// Each step said itself as it happened; what is left to say is that there were none.
fn print_laid(laid: &install::Laid) {
    if laid.done.is_empty() {
        println!("  nothing to change");
    }
}

/// A host with no configuration at all is given the starting point this binary carries and laid out
/// from it in the same run — so what bootstraps this binary ends with a host that is laid out,
/// rather than one that is half way and waiting to be told to finish.
///
/// A configuration that is *there and wrong* is never written over: that is somebody's file, and
/// the only useful thing to do with it is say which key is wrong.
fn configuration(command: &Command) -> Result<(HostConfig, Origin), std::process::ExitCode> {
    let refused = |error: &nibrunnerd::config::ConfigError| {
        match command {
            Command::Serve => tracing::error!(error = %error.message(), "this host is not configured"),
            Command::Install { .. } | Command::Start { .. } => {
                eprintln!("this host is not configured: {}", error.message())
            }
        }
        std::process::ExitCode::FAILURE
    };

    let path = HostConfig::configured_file();
    match HostConfig::load() {
        Ok(config) => Ok((config, Origin::Operator)),
        Err(_) if matches!(command, Command::Install { .. }) && !path.exists() => {
            install::write_starter_configuration(&path).map_err(|error| {
                eprintln!("{}", error.message());
                std::process::ExitCode::FAILURE
            })?;
            println!("  {} written, because this host had none", path.display());
            HostConfig::load()
                .map(|config| (config, Origin::Starter))
                .map_err(|error| refused(&error))
        }
        Err(error) => Err(refused(&error)),
    }
}

async fn serve(config: HostConfig) -> std::process::ExitCode {
    let host = match run::build_host(config).await {
        Ok(host) => host,
        Err(error) => {
            tracing::error!(error = %error.to_string(), "this host could not be brought up");
            return std::process::ExitCode::FAILURE;
        }
    };
    tracing::info!(
        state_dir = %host.config.state_dir.display(),
        desired_state_file = %host.config.desired_state_file.display(),
        guest_memory_mib = host.guest_memory_mib,
        firecracker = nibrunnerd::adapters::vm::process::FIRECRACKER_VERSION,
        "nibrunnerd starting"
    );

    let lifecycle = LifecycleController::new(host.clone(), run::host_versions(&host));
    lifecycle.start().await;
    let running: Vec<_> = lifecycle
        .controllers()
        .into_iter()
        .map(|controller| {
            tracing::info!(controller = controller.name(), "controller started");
            tokio::spawn(async move { controller.run().await })
        })
        .collect();

    ready();
    shutdown().await;
    tracing::info!("nibrunnerd stopping; every microVM on this host keeps running");
    for task in running {
        task.abort();
    }
    lifecycle.stop().await;
    std::process::ExitCode::SUCCESS
}

/// Said to systemd once the host is built and every controller is running, which is what
/// `Type=notify` makes "started" mean: a `systemctl start` that returns is one this daemon
/// serves after, and one that exits binding a listener it was given returns as the failure it is.
/// Nothing is listening when this is not run under systemd, and nothing is said.
#[cfg(target_os = "linux")]
fn ready() {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};

    let Some(socket) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let address = match socket.as_encoded_bytes().strip_prefix(b"@") {
        Some(abstract_name) => SocketAddr::from_abstract_name(abstract_name),
        None => SocketAddr::from_pathname(&socket),
    };
    let sent = address.and_then(|address| {
        let sender = UnixDatagram::unbound()?;
        sender.send_to_addr(b"READY=1", &address)
    });
    if let Err(error) = sent {
        tracing::warn!(error = %error, "systemd was not told this host is ready");
    }
}

#[cfg(not(target_os = "linux"))]
fn ready() {}

#[cfg(unix)]
async fn shutdown() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(terminate) => terminate,
        Err(_) => return std::future::pending().await,
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}

fn install_logger() {
    use tracing_subscriber::prelude::*;
    let filter = tracing_subscriber::EnvFilter::try_from_env("NIBRUNNER_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(std::io::stderr)
                .with_current_span(false),
        )
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Result<Command, String> {
        Command::parse(arguments.iter().map(|argument| (*argument).to_string()))
    }

    #[test]
    fn no_arguments_is_the_daemon_this_has_always_been() {
        assert!(matches!(parse(&[]), Ok(Command::Serve)));
    }

    // Where a release is, is the one thing the script that bootstraps this passes in: everything
    // about where the files in it go is read from the configuration instead.
    #[test]
    fn a_release_is_taken_as_the_url_after_the_flag() {
        let Ok(Command::Install { release, force }) = parse(&["install", "--from", "/tmp/release"]) else {
            panic!("--from names a release");
        };
        assert_eq!(release.as_deref(), Some(std::path::Path::new("/tmp/release")));
        assert!(!force);
    }

    #[test]
    fn a_release_flag_with_nothing_after_it_is_refused_rather_than_taken_as_none() {
        let refused = parse(&["install", "--from"]).unwrap_err();
        assert!(refused.contains("--from takes the directory"), "{refused}");
    }

    #[test]
    fn both_commands_take_force_and_only_install_takes_a_release() {
        assert!(matches!(
            parse(&["install"]),
            Ok(Command::Install {
                force: false,
                release: None
            })
        ));
        assert!(matches!(
            parse(&["install", "--force"]),
            Ok(Command::Install {
                force: true,
                release: None
            })
        ));
        assert!(matches!(parse(&["start"]), Ok(Command::Start { force: false })));
        assert!(matches!(
            parse(&["start", "--force"]),
            Ok(Command::Start { force: true })
        ));
        // `start` lays out from what is already on the host; a release is what bootstraps it.
        let refused = parse(&["start", "--from", "/tmp/release"]).unwrap_err();
        assert!(refused.contains("--from is not an argument"), "{refused}");
    }

    // A mistyped flag that is quietly ignored is a host laid out differently from the way it was
    // asked for, which is the one thing an installer must never do.
    #[test]
    fn anything_else_is_refused_by_name_rather_than_ignored() {
        let refused = parse(&["install", "--fore"]).unwrap_err();
        assert!(refused.contains("--fore"), "{refused}");
        let unknown = parse(&["serve"]).unwrap_err();
        assert!(unknown.contains("serve"), "{unknown}");
    }

    #[test]
    fn asking_for_help_is_answered_with_what_it_takes() {
        let said = parse(&["--help"]).unwrap_err();
        assert!(said.contains("nibrunnerd install"), "{said}");
        assert!(said.contains("nibrunnerd start"), "{said}");
    }
}
