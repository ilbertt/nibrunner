use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(version, about)]
#[command(
    after_help = "Without a command, serve this host using /etc/nibrunner/config.toml.\n\nNIBRUNNER_CONFIG names another configuration file. NIBRUNNER_LOG is a tracing filter."
)]
pub(super) struct Cli {
    #[command(subcommand)]
    pub(super) command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub(super) enum Command {
    #[command(about = "Package a local Linux x86_64 Docker image as a filesystem layer and command")]
    ImportImage {
        #[arg(help = "An image already built or pulled into Docker's local image store")]
        image: String,
        #[arg(
            long,
            value_name = "DIR",
            help = "A new directory for the digest-named layer and import.json"
        )]
        output: PathBuf,
        #[arg(
            long,
            value_name = "PATH",
            help = "Override the image's program with an absolute guest path"
        )]
        program: Option<protocol::GuestPath>,
    },
    #[command(about = "Lay out the guest image, kernel settings, ZeroFS and units without starting them")]
    Install {
        #[arg(long, help = "Replace files these commands did not write")]
        force: bool,
        #[arg(
            long = "from",
            value_name = "DIR",
            help = "Take the guest image from an unpacked release, verifying its digests"
        )]
        release: Option<PathBuf>,
    },
    #[command(about = "Lay out the host and start its systemd units; run after every configuration edit")]
    Start {
        #[arg(long, help = "Replace files these commands did not write")]
        force: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    fn parse(arguments: &[&str]) -> Result<Option<Command>, clap::Error> {
        Cli::try_parse_from(std::iter::once("nibrunnerd").chain(arguments.iter().copied()))
            .map(|cli| cli.command)
    }

    #[test]
    fn no_arguments_leaves_the_daemon_as_the_default() {
        assert!(parse(&[]).unwrap().is_none());
    }

    #[test]
    fn a_release_is_taken_as_the_directory_after_the_flag() {
        let Some(Command::Install { release, force }) =
            parse(&["install", "--from", "/tmp/release"]).unwrap()
        else {
            panic!("--from names a release");
        };
        assert_eq!(release.as_deref(), Some(std::path::Path::new("/tmp/release")));
        assert!(!force);
    }

    #[test]
    fn a_release_flag_with_nothing_after_it_is_refused_rather_than_taken_as_none() {
        let refused = parse(&["install", "--from"]).unwrap_err();
        assert_eq!(refused.exit_code(), 2);
        assert!(refused.to_string().contains("--from"), "{refused}");
    }

    #[test]
    fn both_commands_take_force_and_only_install_takes_a_release() {
        assert!(matches!(
            parse(&["install"]),
            Ok(Some(Command::Install {
                force: false,
                release: None
            }))
        ));
        assert!(matches!(
            parse(&["install", "--force"]),
            Ok(Some(Command::Install {
                force: true,
                release: None
            }))
        ));
        assert!(matches!(
            parse(&["start"]),
            Ok(Some(Command::Start { force: false }))
        ));
        assert!(matches!(
            parse(&["start", "--force"]),
            Ok(Some(Command::Start { force: true }))
        ));
        let refused = parse(&["start", "--from", "/tmp/release"]).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::UnknownArgument);
        assert!(refused.to_string().contains("--from"), "{refused}");
    }

    #[test]
    fn a_release_and_force_can_be_given_in_either_order_or_with_an_equals_sign() {
        for arguments in [
            vec!["install", "--force", "--from", "/tmp/release"],
            vec!["install", "--from", "/tmp/release", "--force"],
            vec!["install", "--from=/tmp/release", "--force"],
        ] {
            let Some(Command::Install { release, force }) = parse(&arguments).unwrap() else {
                panic!("install accepts --from and --force together");
            };
            assert_eq!(release.as_deref(), Some(std::path::Path::new("/tmp/release")));
            assert!(force);
        }
    }

    #[test]
    fn anything_else_is_refused_by_name_rather_than_ignored() {
        for (arguments, unknown) in [
            (vec!["install", "--fore"], "--fore"),
            (vec!["serve"], "serve"),
            (vec!["--force"], "--force"),
        ] {
            let refused = parse(&arguments).unwrap_err();
            assert_eq!(refused.exit_code(), 2);
            assert!(refused.to_string().contains(unknown), "{refused}");
        }
    }

    #[test]
    fn help_is_available_for_the_daemon_and_each_command() {
        for arguments in [
            vec!["--help"],
            vec!["-h"],
            vec!["help"],
            vec!["install", "--help"],
            vec!["start", "--help"],
            vec!["help", "install"],
            vec!["help", "start"],
            vec!["import-image", "--help"],
        ] {
            let help = parse(&arguments).unwrap_err();
            assert_eq!(help.kind(), ErrorKind::DisplayHelp);
            assert_eq!(help.exit_code(), 0);
            assert!(!help.use_stderr());
        }
    }

    #[test]
    fn image_import_requires_an_image_and_a_new_output_directory() {
        assert!(parse(&["import-image", "demo:build"]).is_err());
        assert!(parse(&["import-image", "--output", "./imported"]).is_err());
        let Some(Command::ImportImage {
            image,
            output,
            program,
        }) = parse(&[
            "import-image",
            "demo:build",
            "--output",
            "./imported",
            "--program",
            "/app/server",
        ])
        .unwrap()
        else {
            panic!("import-image accepts a local image and an output directory");
        };
        assert_eq!(image, "demo:build");
        assert_eq!(output, PathBuf::from("./imported"));
        assert_eq!(program.unwrap().as_str(), "/app/server");
        assert!(parse(&[
            "import-image",
            "demo:build",
            "--output",
            "./imported",
            "--program",
            "server"
        ])
        .is_err());
    }

    #[test]
    fn both_version_flags_report_the_compiled_version_successfully() {
        for flag in ["-V", "--version"] {
            let version = parse(&[flag]).unwrap_err();
            assert_eq!(version.kind(), ErrorKind::DisplayVersion);
            assert_eq!(version.exit_code(), 0);
            assert!(!version.use_stderr());
            assert_eq!(
                version.to_string(),
                format!("nibrunnerd {}\n", env!("CARGO_PKG_VERSION"))
            );
        }
    }
}
