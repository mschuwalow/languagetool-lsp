use clap::{Parser, Subcommand};
use languagetool_lsp::config::ClientOptions;
use languagetool_lsp::languagetool::LanguageToolClient;
use languagetool_lsp::lsp::Backend;
use languagetool_lsp::masking::{annotated_for_language, ignored_ranges_for_language};
use std::path::{Path, PathBuf};
use tower_lsp::{LspService, Server};

#[derive(Parser)]
#[command(version, about, long_about = None, arg_required_else_help = true)]
struct Cli {
    /// Root of the workspace/project being checked.
    #[arg(short, long, value_name = "FOLDER")]
    root: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Serve the Language Server Protocol over stdio.
    Serve,
    /// Check whether the configured LanguageTool endpoint responds.
    Health,
    /// Check files and print diagnostics to stdout.
    Check { files: Vec<PathBuf> },
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();
    let root = cli.root.as_deref().unwrap_or_else(|| Path::new("."));

    match cli.command {
        Commands::Serve => serve(root).await,
        Commands::Health => health().await,
        Commands::Check { files } => check(files).await,
    }
}

async fn serve(root: &Path) {
    log::info!("Starting LanguageTool LSP from {}", root.display());
    let (stdin, stdout) = (tokio::io::stdin(), tokio::io::stdout());
    let root = root.to_path_buf();
    let (service, socket) = LspService::new(|client| Backend::new(client, root.clone()));
    Server::new(stdin, stdout, socket).serve(service).await;
}

async fn health() {
    let options = ClientOptions::default();
    let client = LanguageToolClient::new();
    match client.check("This are a tset.", &options).await {
        Ok(response) => {
            let software = response
                .software
                .as_ref()
                .map(|software| format!("{} {}", software.name, software.version))
                .unwrap_or_else(|| "LanguageTool".to_string());
            println!("{software}: {} match(es)", response.matches.len());
        }
        Err(err) => {
            eprintln!("LanguageTool health check failed: {err}");
            std::process::exit(1);
        }
    }
}

async fn check(files: Vec<PathBuf>) {
    let options = ClientOptions::default();
    let client = LanguageToolClient::new();
    let mut had_errors = false;

    for path in files {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                eprintln!("{}: {err}", path.display());
                had_errors = true;
                continue;
            }
        };

        let data = annotated_for_language(&text, language_id_for_path(&path));
        let ignored_ranges = ignored_ranges_for_language(&text, language_id_for_path(&path));
        let mut hits = Vec::new();

        if data.has_text() {
            match client.check_annotated(&data, &options).await {
                Ok(response) => {
                    hits.extend(response.matches.into_iter().filter_map(|item| {
                        let offset = usize::try_from(item.offset).ok()?;
                        let length = usize::try_from(item.length).ok()?;
                        if utf16_text_for_range(&text, offset, offset + length)
                            .trim()
                            .is_empty()
                            || intersects_ignored_ranges(offset, offset + length, &ignored_ranges)
                        {
                            None
                        } else {
                            Some((offset, length, item.message))
                        }
                    }));
                }
                Err(err) => {
                    eprintln!("{}: {err}", path.display());
                    had_errors = true;
                }
            }
        }

        if !hits.is_empty() {
            println!("{}", path.display());
            for (offset, length, message) in hits {
                println!("  {offset}:{length} {message}");
            }
        }
    }

    if had_errors {
        std::process::exit(1);
    }
}

fn intersects_ignored_ranges(start: usize, end: usize, ignored_ranges: &[(usize, usize)]) -> bool {
    ignored_ranges
        .iter()
        .any(|(ignored_start, ignored_end)| start < *ignored_end && end > *ignored_start)
}

fn language_id_for_path(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("c") | Some("h") => Some("c"),
        Some("cc") | Some("cpp") | Some("cxx") | Some("hpp") => Some("cpp"),
        Some("css") => Some("css"),
        Some("go") => Some("go"),
        Some("html") | Some("htm") => Some("html"),
        Some("java") => Some("java"),
        Some("js") | Some("jsx") => Some("javascript"),
        Some("md") | Some("markdown") => Some("markdown"),
        Some("mdx") => Some("mdx"),
        Some("py") => Some("python"),
        Some("rs") => Some("rust"),
        Some("sh") | Some("bash") => Some("shell"),
        Some("ts") | Some("tsx") => Some("typescript"),
        _ => None,
    }
}

fn utf16_text_for_range(text: &str, start: usize, end: usize) -> String {
    let mut offset = 0;
    let mut output = String::new();

    for ch in text.chars() {
        let next = offset + ch.len_utf16();
        if offset >= start && next <= end {
            output.push(ch);
        }
        offset = next;
        if offset >= end {
            break;
        }
    }

    output
}
