use clap::{Parser, Subcommand};
use languagetool_lsp::config::{ClientOptions, ProjectConfig};
use languagetool_lsp::document::Document;
use languagetool_lsp::languagetool::LanguageToolClient;
use languagetool_lsp::lsp::Backend;
use std::path::{Path, PathBuf};
use tower_lsp::lsp_types::Url;
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
        Commands::Health => health(root).await,
        Commands::Check { files } => check(root, files).await,
    }
}

async fn serve(root: &Path) {
    log::info!("Starting LanguageTool LSP from {}", root.display());
    let (stdin, stdout) = (tokio::io::stdin(), tokio::io::stdout());
    let root = root.to_path_buf();
    let (service, socket) = LspService::new(|client| Backend::new(client, root.clone()));
    Server::new(stdin, stdout, socket).serve(service).await;
}

async fn cli_options(root: &Path) -> ClientOptions {
    let options = ClientOptions::default();
    ProjectConfig::load(&options.project_config_path(root))
        .await
        .merged_options(&options)
}

async fn health(root: &Path) {
    log::info!("Checking LanguageTool health");
    let options = cli_options(root).await;
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

async fn check(root: &Path, files: Vec<PathBuf>) {
    let options = cli_options(root).await;
    let client = LanguageToolClient::new();
    let mut had_errors = false;
    log::info!("Checking {} file(s) with LanguageTool", files.len());

    for path in files {
        log::debug!("Reading {}", path.display());
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(err) => {
                log::debug!("Failed to read {}: {err}", path.display());
                eprintln!("{}: {err}", path.display());
                had_errors = true;
                continue;
            }
        };

        let uri = match file_uri(&path) {
            Ok(uri) => uri,
            Err(err) => {
                log::debug!("Failed to build file URI for {}: {err}", path.display());
                eprintln!("{}: {err}", path.display());
                had_errors = true;
                continue;
            }
        };
        log::debug!(
            "Preparing CLI document for {} uri={uri} bytes={}",
            path.display(),
            text.len()
        );
        let document = Document::new(uri, 0, None, text);
        let Some(checkable_document) =
            document.checkable(|language| options.language_enabled(&language))
        else {
            log::debug!("Skipping {} because it is not checkable", path.display());
            continue;
        };
        let mut hits = Vec::new();

        log::debug!(
            "Sending LanguageTool request for {} annotations={} ignored_ranges={}",
            path.display(),
            checkable_document.annotated().annotation.len(),
            checkable_document.ignored_ranges().len()
        );
        match client
            .check_annotated(checkable_document.annotated(), &options)
            .await
        {
            Ok(response) => {
                log::debug!(
                    "LanguageTool returned {} match(es) for {}",
                    response.matches.len(),
                    path.display()
                );
                hits.extend(response.matches.into_iter().filter_map(|item| {
                    let offset = usize::try_from(item.offset).ok()?;
                    let length = usize::try_from(item.length).ok()?;
                    let matched_text = checkable_document.index().text_for_utf16_range(
                        checkable_document.text(),
                        offset,
                        offset + length,
                    )?;
                    if matched_text.trim().is_empty()
                        || options.is_ignored_word(matched_text)
                        || intersects_ignored_ranges(
                            offset,
                            offset + length,
                            checkable_document.ignored_ranges(),
                        )
                    {
                        None
                    } else {
                        Some((offset, length, item.message))
                    }
                }));
            }
            Err(err) => {
                log::debug!("LanguageTool check failed for {}: {err}", path.display());
                eprintln!("{}: {err}", path.display());
                had_errors = true;
            }
        }

        if !hits.is_empty() {
            log::info!("{} produced {} hit(s)", path.display(), hits.len());
            println!("{}", path.display());
            for (offset, length, message) in hits {
                println!("  {offset}:{length} {message}");
            }
        } else {
            log::debug!("{} produced no reportable hits", path.display());
        }
    }

    if had_errors {
        std::process::exit(1);
    }
}

fn file_uri(path: &Path) -> Result<Url, String> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|err| format!("failed to resolve current directory: {err}"))?
            .join(path)
    };

    Url::from_file_path(&path).map_err(|()| "failed to convert path to file URI".to_string())
}

fn intersects_ignored_ranges(start: usize, end: usize, ignored_ranges: &[(usize, usize)]) -> bool {
    ignored_ranges
        .iter()
        .any(|(ignored_start, ignored_end)| start < *ignored_end && end > *ignored_start)
}
