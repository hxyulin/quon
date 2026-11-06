// Prints its arguments and exits with the status named by HELLO_EXIT,
// so the CLI tests can see both what `quon run` passed and how it
// judged the exit.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    println!("hello {}", args.join(" "));
    let code = std::env::var("HELLO_EXIT").map_or(0, |c| c.parse().unwrap());
    std::process::exit(code);
}
