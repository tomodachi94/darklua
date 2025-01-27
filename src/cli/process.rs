use crate::cli::error::CliError;
use crate::cli::utils::maybe_plural;
use crate::cli::{CommandResult, GlobalOptions};

use clap::Args;
use darklua_core::{GeneratorParameters, Resources};
#[cfg(not(target_arch = "wasm32"))]
use notify::RecursiveMode;
#[cfg(not(target_arch = "wasm32"))]
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, DebouncedEventKind};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const FILE_WATCHING_DEBOUNCE_DURATION_MILLIS: u64 = 400;

#[derive(Debug, Args, Clone)]
pub struct Options {
    /// Path to the lua file to process.
    input_path: PathBuf,
    /// Where to output the result.
    output_path: PathBuf,
    /// Choose a specific configuration file.
    #[arg(long, short, alias = "config-path")]
    config: Option<PathBuf>,
    /// Choose how Lua code is formatted ('dense', 'readable' or 'retain_lines').
    /// This will override the format given by the configuration file.
    #[arg(long)]
    format: Option<LuaFormat>,
    /// Watch files and directories for changes and automatically re-run
    #[arg(long, short)]
    watch: bool,
}

#[derive(Debug, Copy, Clone)]
enum LuaFormat {
    Dense,
    Readable,
    RetainLines,
}

impl FromStr for LuaFormat {
    type Err = String;

    fn from_str(format: &str) -> Result<Self, Self::Err> {
        match format {
            "dense" => Ok(Self::Dense),
            "readable" => Ok(Self::Readable),
            // keep "retain-lines" for back-compatibility
            "retain_lines" | "retain-lines" => Ok(Self::RetainLines),
            _ => Err(format!(
                "format '{}' does not exist! (possible options are: 'dense', 'readable' or 'retain_lines'",
                format
            )),
        }
    }
}

fn process(resources: Resources, process_options: darklua_core::Options) -> Result<(), CliError> {
    let process_start_time = Instant::now();

    let result = darklua_core::process(&resources, process_options);

    let process_duration = durationfmt::to_string(process_start_time.elapsed());

    let success_count = result.success_count();

    match result.result() {
        Ok(()) => {
            println!(
                "successfully processed {} file{} (in {})",
                success_count,
                maybe_plural(success_count),
                process_duration
            );
            Ok(())
        }
        Err(errors) => {
            let error_count = errors.len();
            if success_count > 0 {
                eprintln!(
                    "successfully processed {} file{} (in {})",
                    success_count,
                    maybe_plural(success_count),
                    process_duration
                );
                eprintln!(
                    "but {} error{} happened:",
                    error_count,
                    maybe_plural(error_count)
                );
            } else {
                eprintln!(
                    "{} error{} happened:",
                    error_count,
                    maybe_plural(error_count)
                );
            }

            errors.iter().for_each(|error| eprintln!("-> {}", error));

            Err(CliError::new(1))
        }
    }
}

impl Options {
    fn process(&self) -> Result<(), CliError> {
        let resources = Resources::from_file_system();

        let mut process_options =
            darklua_core::Options::new(&self.input_path).with_output(&self.output_path);

        if let Some(config) = self.config.as_ref() {
            process_options = process_options.with_configuration_at(config);
        }

        if let Some(format) = self.format {
            process_options = process_options.with_generator_override(match format {
                LuaFormat::Dense => GeneratorParameters::default_dense(),
                LuaFormat::Readable => GeneratorParameters::default_readable(),
                LuaFormat::RetainLines => GeneratorParameters::RetainLines,
            })
        }

        process(resources, process_options)
    }
}

const DEFAULT_CONFIG_PATHS: [&str; 2] = [".darklua.json", ".darklua.json5"];

pub fn run(options: &Options, _global: &GlobalOptions) -> CommandResult {
    log::debug!("running `process`: {:?}", options);

    if cfg!(not(target_arch = "wasm32")) && options.watch {
        // run process once initially
        if let Err(_cli_err) = options.process() {
            // ignore error darklua need to setup watch mode
        }

        let watcher_options = options.clone();

        let mut debouncer = new_debouncer(
            Duration::from_millis(FILE_WATCHING_DEBOUNCE_DURATION_MILLIS),
            move |events: DebounceEventResult| {
                match events {
                    Ok(events) => {
                        if events
                            .iter()
                            .any(|event| matches!(event.kind, DebouncedEventKind::Any))
                        {
                            log::debug!("changes detected, re-running process");
                            if let Err(_cli_err) = watcher_options.process() {
                                // ignore error since it already has been printed
                            }
                        }
                    }
                    Err(err) => {
                        log::error!(
                            "an error occured while watching file system for changes: {}",
                            err
                        );
                    }
                }
            },
        )
        .map_err(|err| {
            log::error!("unable to create file watcher: {}", err);
            CliError::new(1)
        })?;

        log::debug!(
            "start watching file system {}",
            options.input_path.display()
        );

        debouncer
            .watcher()
            .watch(&options.input_path, RecursiveMode::Recursive)
            .map_err(|err| {
                log::error!(
                    "unable to start watching file system at `{}`: {}",
                    options.input_path.display(),
                    err
                );
                CliError::new(1)
            })?;

        if let Some(config) = options.config.as_ref() {
            log::debug!("start watching provided config path {}", config.display());
            debouncer
                .watcher()
                .watch(config, RecursiveMode::NonRecursive)
                .map_err(|err| {
                    log::error!(
                        "unable to start watching file system at `{}`: {}",
                        config.display(),
                        err
                    );
                    CliError::new(1)
                })?;
        } else {
            for path in DEFAULT_CONFIG_PATHS.iter().map(Path::new) {
                if path.exists() {
                    log::debug!("start watching default config path {}", path.display());
                    debouncer
                        .watcher()
                        .watch(path, RecursiveMode::NonRecursive)
                        .map_err(|err| {
                            log::error!(
                                "unable to start watching file system at `{}`: {}",
                                path.display(),
                                err
                            );
                            CliError::new(1)
                        })?;
                }
            }
        }

        let (sender, receiver) = mpsc::channel();

        ctrlc::set_handler(move || sender.send(()).expect("unable to send signal to terminate"))
            .map_err(|err| {
                log::error!("unable to set Ctrl-C handler: {}", err);
                CliError::new(1)
            })?;

        log::debug!("waiting for Ctrl-C to close the program");
        receiver.recv().expect("Could not receive from channel.");

        Ok(())
    } else {
        options.process()
    }
}
