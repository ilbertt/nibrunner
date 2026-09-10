#![cfg_attr(test, allow(clippy::unwrap_used, clippy::panic, clippy::expect_used))]

use nibrunnerd::config::HostConfig;
use nibrunnerd::controllers::lifecycle_controller::LifecycleController;
use nibrunnerd::{install, run};

/// The two things this binary is ever asked to do. `install` lays a host out; everything else this
/// daemon does, it does by serving — and neither reads anything the other does not.
#[derive(Debug)]
enum Command {
    Serve,
    Install { force: bool, release: Option<String> },
}

const USAGE: &str = "\
nibrunnerd — one binary that turns a Linux machine into an app host.

    nibrunnerd                 serve this host, reading /etc/nibrunner/config.toml
    nibrunnerd install         lay this host out from that same file, then exit
    nibrunnerd install --force replace files `install` did not write
    nibrunnerd install --from <url>
                               take the guest image from a release at this URL

`--from` is what `deploy/install.sh` passes: it names where a release is, and everything
about where the files in it go is read from the configuration rather than told twice.

NIBRUNNER_CONFIG names another configuration file. NIBRUNNER_LOG is a tracing filter.
";

impl Command {
    fn install(arguments: &[String]) -> Result<Self, String> {
        let mut force = false;
        let mut release = None;
        let mut rest = arguments.iter();
        while let Some(argument) = rest.next() {
            match argument.as_str() {
                "--force" => force = true,
                "--from" => {
                    release = Some(rest.next().cloned().ok_or_else(|| {
                        format!("nibrunnerd install: --from takes the URL of a release\n\n{USAGE}")
                    })?);
                }
                unknown => {
                    return Err(format!(
                        "nibrunnerd install: {unknown} is not an argument it takes\n\n{USAGE}"
                    ))
                }
            }
        }
        Ok(Self::Install { force, release })
    }

    fn parse(arguments: impl Iterator<Item = String>) -> Result<Self, String> {
        let arguments: Vec<String> = arguments.collect();
        match arguments.split_first() {
            None => Ok(Self::Serve),
            Some((first, rest)) if first == "install" => Self::install(rest),
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

    let config = match HostConfig::load() {
        Ok(config) => config,
        Err(error) => {
            match command {
                Command::Serve => tracing::error!(error = %error.message(), "this host is not configured"),
                Command::Install { .. } => return no_configuration(&error),
            }
            return std::process::ExitCode::FAILURE;
        }
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
        Command::Install { force, release } => runtime.block_on(lay_out(config, force, release)),
    }
}

async fn lay_out(config: HostConfig, force: bool, release: Option<String>) -> std::process::ExitCode {
    let config_file = HostConfig::configured_file();
    match install::run(&config, &config_file, force, release.as_deref()).await {
        Ok(laid) => {
            for step in laid.steps {
                println!("  {step}");
            }
            print!("{}", install::what_is_left(&config, &config_file));
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{}", error.message());
            std::process::ExitCode::FAILURE
        }
    }
}

/// A host with no configuration is given one to start from rather than a refusal it has to go and
/// find the answer to. A host whose configuration is *there and wrong* is never overwritten.
fn no_configuration(error: &nibrunnerd::config::ConfigError) -> std::process::ExitCode {
    let path = HostConfig::configured_file();
    if path.exists() {
        eprintln!("this host is not configured: {}", error.message());
        return std::process::ExitCode::FAILURE;
    }
    match install::write_starter_configuration(&path) {
        Ok(()) => {
            println!("  {} written as a starting point", path.display());
            println!(
                "\nThis host had no configuration, so it has one now: volumes as files on its own\n\
                 disk, no proxy, no object store. Read it, make it this host's, and run\n\
                 `nibrunnerd install` again."
            );
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{}", error.message());
            std::process::ExitCode::FAILURE
        }
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

    shutdown().await;
    tracing::info!("nibrunnerd stopping; every microVM on this host keeps running");
    for task in running {
        task.abort();
    }
    lifecycle.stop().await;
    std::process::ExitCode::SUCCESS
}

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
        let Ok(Command::Install { release, force }) = parse(&["install", "--from", "https://example.test/r"])
        else {
            panic!("--from names a release");
        };
        assert_eq!(release.as_deref(), Some("https://example.test/r"));
        assert!(!force);
    }

    #[test]
    fn a_release_flag_with_nothing_after_it_is_refused_rather_than_taken_as_none() {
        let refused = parse(&["install", "--from"]).unwrap_err();
        assert!(refused.contains("--from takes the URL"), "{refused}");
    }

    #[test]
    fn install_is_the_only_command_and_force_the_only_flag() {
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
    }
}
