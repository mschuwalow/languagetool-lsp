{
  mkShell,
  rust-bin,
  cargo-make,
  cargo-release,
  nodejs,
}:

let
  rust-toolchain = rust-bin.stable.latest.default.override {
    extensions = [
      "rust-src"
      "rustfmt"
      "clippy"
      "rust-analyzer"
    ];
  };
in

mkShell {
  nativeBuildInputs = [
    rust-toolchain
    cargo-make
    cargo-release
    nodejs
  ];
}
