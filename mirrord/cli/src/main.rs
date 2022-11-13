use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use config::*;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use errno;
use exec::execvp;
use mirrord_auth::AuthConfig;
use mirrord_progress::TaskProgress;
#[cfg(target_os = "macos")]
use mirrord_sip::sip_patch;
use rand::distributions::{Alphanumeric, DistString};
use semver::Version;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{fmt, prelude::*, registry, EnvFilter};

mod config;

#[cfg(target_os = "linux")]
const INJECTION_ENV_VAR: &str = "LD_PRELOAD";

#[cfg(target_os = "macos")]
const INJECTION_ENV_VAR: &str = "DYLD_INSERT_LIBRARIES";

/// For some reason loading dylib from $TMPDIR can get the process killed somehow..?
#[cfg(target_os = "macos")]
mod mac {
    use std::str::FromStr;

    use super::*;

    pub fn temp_dir() -> PathBuf {
        PathBuf::from_str("/tmp/").unwrap()
    }
}

#[cfg(not(target_os = "macos"))]
use std::env::temp_dir;

#[cfg(target_os = "macos")]
use mac::temp_dir;

fn extract_library(dest_dir: Option<String>) -> Result<PathBuf> {
    let progress = TaskProgress::new("initializing mirrord layer...");
    let library_file = env!("MIRRORD_LAYER_FILE");
    let library_path = Path::new(library_file);

    let extension = library_path
        .components()
        .last()
        .unwrap()
        .as_os_str()
        .to_str()
        .unwrap()
        .split('.')
        .collect::<Vec<&str>>()[1];

    let file_name = format!(
        "{}-libmirrord_layer.{extension}",
        Alphanumeric
            .sample_string(&mut rand::thread_rng(), 10)
            .to_lowercase()
    );

    let file_path = match dest_dir {
        Some(dest_dir) => std::path::Path::new(&dest_dir).join(file_name),
        None => temp_dir().as_path().join(file_name),
    };
    let mut file = File::create(&file_path)
        .with_context(|| format!("Path \"{}\" creation failed", file_path.display()))?;
    let bytes = include_bytes!(env!("MIRRORD_LAYER_FILE"));
    file.write_all(bytes).unwrap();

    debug!("Extracted library file to {:?}", &file_path);
    progress.done_with("layer initialized");
    Ok(file_path)
}

fn add_to_preload(path: &str) -> Result<()> {
    match std::env::var(INJECTION_ENV_VAR) {
        Ok(value) => {
            let new_value = format!("{}:{}", value, path);
            std::env::set_var(INJECTION_ENV_VAR, new_value);
            Ok(())
        }
        Err(std::env::VarError::NotPresent) => {
            std::env::set_var(INJECTION_ENV_VAR, path);
            Ok(())
        }
        Err(e) => {
            error!("Failed to set environment variable with error {:?}", e);
            Err(anyhow!("Failed to set environment variable"))
        }
    }
}

fn exec(args: &ExecArgs) -> Result<()> {
    if !args.no_telemetry {
        prompt_outdated_version();
    }
    info!(
        "Launching {:?} with arguments {:?}",
        args.binary, args.binary_args
    );

    if !(args.no_tcp_outgoing || args.no_udp_outgoing) && args.no_remote_dns {
        warn!("TCP/UDP outgoing enabled without remote DNS might cause issues when local machine has IPv6 enabled but remote cluster doesn't")
    }

    if let Some(target) = &args.target {
        std::env::set_var("MIRRORD_IMPERSONATED_TARGET", target);
    }

    if let Some(skip_processes) = &args.skip_processes {
        std::env::set_var("MIRRORD_SKIP_PROCESSES", skip_processes.clone());
    }

    if let Some(namespace) = &args.target_namespace {
        std::env::set_var("MIRRORD_TARGET_NAMESPACE", namespace.clone());
    }

    if let Some(namespace) = &args.agent_namespace {
        std::env::set_var("MIRRORD_AGENT_NAMESPACE", namespace.clone());
    }

    if let Some(log_level) = &args.agent_log_level {
        std::env::set_var("MIRRORD_AGENT_RUST_LOG", log_level.clone());
    }

    if let Some(image) = &args.agent_image {
        std::env::set_var("MIRRORD_AGENT_IMAGE", image.clone());
    }

    if let Some(agent_ttl) = &args.agent_ttl {
        std::env::set_var("MIRRORD_AGENT_TTL", agent_ttl.to_string());
    }
    if let Some(agent_statup_timeout) = &args.agent_statup_timeout {
        std::env::set_var(
            "MIRRORD_AGENT_STARTUP_TIMEOUT",
            agent_statup_timeout.to_string(),
        );
    }

    if args.enable_rw_fs && args.no_fs {
        warn!("fs was both enabled and disabled - disabling will take precedence.");
    }

    if !args.no_fs && args.enable_rw_fs {
        std::env::set_var("MIRRORD_FILE_OPS", "true");
    }

    if args.no_fs || args.enable_rw_fs {
        std::env::set_var("MIRRORD_FILE_RO_OPS", "false");
    }

    if let Some(override_env_vars_exclude) = &args.override_env_vars_exclude {
        std::env::set_var(
            "MIRRORD_OVERRIDE_ENV_VARS_EXCLUDE",
            override_env_vars_exclude,
        );
    }

    if let Some(override_env_vars_include) = &args.override_env_vars_include {
        std::env::set_var(
            "MIRRORD_OVERRIDE_ENV_VARS_INCLUDE",
            override_env_vars_include,
        );
    }

    if args.no_remote_dns {
        std::env::set_var("MIRRORD_REMOTE_DNS", "false");
    }

    if args.accept_invalid_certificates {
        std::env::set_var("MIRRORD_ACCEPT_INVALID_CERTIFICATES", "true");
    }

    if args.ephemeral_container {
        std::env::set_var("MIRRORD_EPHEMERAL_CONTAINER", "true");
    };

    if args.tcp_steal {
        std::env::set_var("MIRRORD_AGENT_TCP_STEAL_TRAFFIC", "true");
    };

    if args.no_outgoing || args.no_tcp_outgoing {
        std::env::set_var("MIRRORD_TCP_OUTGOING", "false");
    }

    if args.no_outgoing || args.no_udp_outgoing {
        std::env::set_var("MIRRORD_UDP_OUTGOING", "false");
    }

    if let Some(config_file) = &args.config_file {
        std::env::set_var("MIRRORD_CONFIG_FILE", config_file.clone());
    }

    if args.capture_error_trace {
        std::env::set_var("MIRRORD_CAPTURE_ERROR_TRACE", "true");
    }

    let library_path = extract_library(args.extract_path.clone())?;
    add_to_preload(library_path.to_str().unwrap()).unwrap();

    #[cfg(target_os = "macos")]
    let binary = sip_patch(&args.binary)?;
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    let did_sip_patch = &binary != &args.binary;

    #[cfg(not(target_os = "macos"))]
    let binary = args.binary.clone();

    let mut binary_args = args.binary_args.clone();
    binary_args.insert(0, binary.clone());

    let err = execvp(binary, binary_args);
    error!("Couldn't execute {:?}", err);
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    if let exec::Error::Errno(errno::Errno(86)) = err {
        // "Bad CPU type in executable"
        if did_sip_patch {
            // We did a SIP patch.
            error!(
                "The file you are trying to run, {}, is either SIP protected or a script with a
                shebang that leads to a SIP protected binary. In order to bypass SIP protection,
                mirrord creates a non-SIP version of the binary and runs that one instead of the
                protected one. The non-SIP version is however an x86_64 file, so in order to run
                it on apple hardware, rosetta has to be installed.
                Rosetta can be installed by runnning:

                softwareupdate --install-rosetta

                ",
                &args.binary
            )
        }
    }
    Err(anyhow!("Failed to execute binary"))
}

#[allow(dead_code)]
fn login(args: LoginArgs) -> Result<()> {
    match &args.token {
        Some(token) => AuthConfig::from_input(token)?.save()?,
        None => {
            AuthConfig::from_webbrowser(&args.auth_server, args.timeout, args.no_open)?.save()?
        }
    }

    println!(
        "Config succesfuly saved at {}",
        AuthConfig::config_path().display()
    );

    Ok(())
}

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
fn main() -> Result<()> {
    registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    match cli.commands {
        Commands::Exec(args) => exec(&args)?,
        Commands::Extract { path } => {
            extract_library(Some(path))?;
        } // Commands::Login(args) => login(args)?,
    }
    Ok(())
}

fn prompt_outdated_version() {
    let check_version: bool = std::env::var("MIRRORD_CHECK_VERSION")
        .map(|s| s.parse().unwrap_or(true))
        .unwrap_or(true);

    if check_version {
        if let Ok(client) = reqwest::blocking::Client::builder().build() {
            if let Ok(result) = client
                .get(format!(
                    "https://version.mirrord.dev/get-latest-version?source=2&currentVersion={}&platform={}",
                    CURRENT_VERSION,
                    std::env::consts::OS
                ))
                .timeout(Duration::from_secs(1))
                .send()
            {
                if let Ok(latest_version) = Version::parse(&result.text().unwrap()) {
                    if latest_version > Version::parse(CURRENT_VERSION).unwrap() {
                        println!("New mirrord version available: {}. To update, run: `curl -fsSL https://raw.githubusercontent.com/metalbear-co/mirrord/main/scripts/install.sh | bash`.", latest_version);
                        println!("To disable version checks, set env variable MIRRORD_CHECK_VERSION to 'false'.")
                    }
                }
            }
        }
    }
}