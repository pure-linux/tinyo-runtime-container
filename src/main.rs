mod utils;

fn main() {
    if let Err(e) = utils::core::run() {
        eprintln!("Error: {}", e);
    }
}