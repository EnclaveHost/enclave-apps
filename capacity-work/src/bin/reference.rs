use std::io::{self, Read};
fn main() {
    let mut input = String::new();
    io::stdin().take(4097).read_to_string(&mut input).unwrap();
    if input.len() > 4096 {
        eprintln!("request too large");
        std::process::exit(1);
    }
    let result = serde_json::from_str::<capacity_work::Work>(&input)
        .map_err(|e| e.to_string())
        .and_then(|w| capacity_work::run(&w));
    match result {
        Ok(r) => println!("{}", serde_json::to_string(&r).unwrap()),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
