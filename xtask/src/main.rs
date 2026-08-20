fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if let Err(error) = xtask::run(&arguments) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
