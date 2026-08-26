//! CLI entry point for lite-encoder (`liteenc`).
//!
//! Full demux / transcode / mux wiring is on the roadmap; this binary exists
//! so the official tool name is stable.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && matches!(args[1].as_str(), "-h" | "--help" | "help") {
        print_help();
        return;
    }
    if args.len() > 1 && matches!(args[1].as_str(), "-V" | "--version" | "version") {
        println!("liteenc {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    eprintln!("liteenc: transcode CLI not implemented yet.\n");
    print_help();
    std::process::exit(2);
}

fn print_help() {
    eprintln!(
        "usage: liteenc [OPTIONS] -i INPUT -o OUTPUT\n\
         \n\
         Rust ffmpeg alternative (work in progress).\n\
         \n\
         Planned:\n\
           liteenc -i input.mp4 -c:v av1 -c:a copy output.webm\n\
           liteenc -i input.h264 -c copy output.mkv\n\
           liteenc probe input.mkv\n\
         \n\
         Library and examples work today; see README.md."
    );
}
