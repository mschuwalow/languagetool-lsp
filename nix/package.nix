{
  lib,
  makeRustPlatform,
  rust-bin,
}:

let
  rustPlatform = makeRustPlatform {
    cargo = rust-bin.stable.latest.minimal;
    rustc = rust-bin.stable.latest.minimal;
  };
  cargoToml = lib.importTOML ../crates/languagetool-lsp/Cargo.toml;
in

rustPlatform.buildRustPackage {
  pname = cargoToml.package.name;
  version = cargoToml.package.version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.gitTracked ../.;
  };

  cargoLock.lockFile = ../Cargo.lock;

  cargoBuildFlags = [
    "--package"
    "languagetool-lsp"
  ];

  # The workspace's integration tests talk to a real LanguageTool HTTP
  # server, which isn't available in the build sandbox.
  doCheck = false;

  meta = {
    description = cargoToml.package.description;
    homepage = "https://github.com/mschuwalow/languagetool-lsp";
    license = lib.licenses.mit;
    mainProgram = "languagetool-lsp";
  };
}
