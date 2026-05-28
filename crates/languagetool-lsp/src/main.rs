use clap::{Parser, Subcommand};
use languagetool_lsp::lsp::Backend;
use std::path::{Path, PathBuf};
use tower_lsp::{LspService, Server};

#[derive(Parser)]
#[command(version, about, long_about = None, arg_required_else_help = true)]
struct Cli {
    /// Root of the workspace/project being served.
    #[arg(short, long, value_name = "FOLDER")]
    root: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Serve the Language Server Protocol over stdio.
    Serve,
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();
    let root = cli.root.as_deref().unwrap_or_else(|| Path::new("."));

    match cli.command {
        Commands::Serve => serve(root).await,
    }
}

async fn serve(root: &Path) {
    log::info!("Starting LanguageTool LSP from {}", root.display());
    let (stdin, stdout) = (tokio::io::stdin(), tokio::io::stdout());
    let root = root.to_path_buf();
    let (service, socket) = LspService::new(|client| Backend::new(client, root.clone()));
    Server::new(stdin, stdout, socket).serve(service).await;
}
