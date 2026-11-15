use hardroc::server::Backend;
use tower_lsp::{LspService, Server};

#[tokio::main]
async fn main() {
    let args = std::env::args().skip(1);
    for arg in args {
        match arg.as_str() {
            "--version" | "-V" | "version" => {
                println!("hardroc {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--help" | "-h" => {
                print_help();
                return;
            }
            "--stdio" => {}
            other => {
                eprintln!("hardroc: unknown argument `{other}`");
                eprintln!("run `hardroc --help` for usage");
                std::process::exit(2);
            }
        }
    }

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(Backend::new);
    Server::new(stdin, stdout, socket).serve(service).await;
}

fn print_help() {
    println!(
        "hardroc {}\n\
         \n\
         A language server for the Roc programming language.\n\
         \n\
         USAGE:\n\
             hardroc --stdio\n\
         \n\
         OPTIONS:\n\
             --stdio       Communicate over stdin/stdout (the only transport)\n\
             --version     Print the version\n\
             --help        Print this help\n\
         \n\
         The server reads JSON-RPC with Content-Length framing, as required by\n\
         the Language Server Protocol. Editors such as Helix start it for you.\n",
        env!("CARGO_PKG_VERSION")
    );
}
