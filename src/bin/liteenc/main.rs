//! `liteenc`: the lite-encoder command line tool.
//!
//! ffmpeg-shaped on purpose — `-i`, `-c:v`, `-c copy`, and a `probe`
//! subcommand — because that is the muscle memory anyone reaching for this
//! already has. Argument parsing lives in [`args`], the work in [`run`].
//!
//! Exit codes are part of the interface; see [`args::USAGE`].

mod args;
mod run;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    let command = match args::parse(&argv) {
        Ok(command) => command,
        Err(e) => {
            eprintln!("liteenc: {e}");
            eprintln!();
            eprintln!("{}", args::USAGE);
            std::process::exit(run::EXIT_USAGE);
        }
    };

    if verbose(&command) {
        init_logging();
    }

    let code = match run::run(command) {
        Ok(()) => run::EXIT_OK,
        Err(failure) => {
            eprintln!("liteenc: {}", failure.message);
            failure.code
        }
    };

    // `process::exit` skips the flush that dropping stdout would do, and
    // `probe` output is what a caller is most likely piping somewhere.
    let _ = std::io::Write::flush(&mut std::io::stdout());
    std::process::exit(code);
}

fn verbose(command: &args::Command) -> bool {
    match command {
        args::Command::Probe(a) => a.verbose,
        args::Command::Transcode(a) => a.verbose,
        _ => false,
    }
}

/// Send the crate's `tracing` output to stderr. `RUST_LOG` wins if it is set,
/// so `-v` is a shorthand rather than an override.
fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("lite_encoder=debug,liteenc=debug"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
