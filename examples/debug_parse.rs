use hardroc::syntax::parse;

fn main() {
    let path = std::env::args().nth(1).expect("path");
    let text = std::fs::read_to_string(&path).expect("read");
    let parsed = parse(&text);
    println!("diagnostics: {:#?}", parsed.diagnostics);
    println!("tokens:");
    for token in &parsed.tokens {
        if !token.kind.is_trivia() {
            println!(
                "{:?} {:?} {:?}",
                token.kind,
                token.keyword,
                token.text(&text)
            );
        }
    }
    println!("module: {:#?}", parsed.module);
}
