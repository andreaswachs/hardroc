use hardroc::syntax::parse;
fn main() {
    let src = std::fs::read_to_string(std::env::args().nth(1).unwrap()).unwrap();
    let parsed = parse(&src);
    println!("DIAGS {:#?}", parsed.diagnostics);
    println!("{:#?}", parsed.module.items);
}
