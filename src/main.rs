mod app;
mod config;
mod db;
mod event;
mod export;
mod harness;
mod probe;
mod sql;
mod types;
mod ui;

const USAGE: &str = "\
osage — terminal SQL IDE

  osage                                  launch the IDE
  osage --probe <driver> <uri> [sql]     headless: dump the catalog as JSON,
                                         then run [sql] and print the rows
  osage --help
";

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => app::run(),
        Some("--help" | "-h") => {
            print!("{USAGE}");
            Ok(())
        }
        Some("--probe") => match args.get(1..) {
            Some([driver, uri, rest @ ..]) => {
                probe::run(driver, uri, rest.first().map(String::as_str))
            }
            _ => {
                eprint!("{USAGE}");
                std::process::exit(2);
            }
        },
        Some(other) => {
            eprintln!("unknown argument: {other}\n");
            eprint!("{USAGE}");
            std::process::exit(2);
        }
    }
}
