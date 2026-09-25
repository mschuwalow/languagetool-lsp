{
  mkShell,
  rust-bin,
  cargo-make,
  cargo-release,
  openssl,
  pkg-config
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
    pkg-config
  ];

  buildInputs = [
    openssl
  ];
}
