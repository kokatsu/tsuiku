use std::path::PathBuf;

fn main() {
    if let Err(error) = run() {
        eprintln!("tsuiku: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), tsuiku::app::AppError> {
    let path = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    tsuiku::app::App::run_path(&path)
}
