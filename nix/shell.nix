{
  lib,
  stdenv,
  pkgsCross,
  mkShell,
  rust-bin,
  cargo-make,
  cargo-release,
  nodejs,
  python3,
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
    targets = lib.optionals stdenv.hostPlatform.isLinux [
      "x86_64-unknown-linux-musl"
    ];
  };

  # cc-rs (used transitively via aws-lc-sys) needs a real musl-targeting
  # C compiler, not just rustc's musl target std lib. Only meaningful on
  # Linux hosts; this is a cross-libc (not cross-arch) toolchain, which
  # nixpkgs supports well.
  musl-cc = pkgsCross.musl64.stdenv.cc;
in

mkShell (
  {
    nativeBuildInputs = [
      rust-toolchain
      cargo-make
      cargo-release
      nodejs
      python3
      pkg-config
    ] ++ lib.optionals stdenv.hostPlatform.isLinux [ musl-cc ];

    buildInputs = [
      openssl
    ];
  }
  // lib.optionalAttrs stdenv.hostPlatform.isLinux {
    CC_x86_64_unknown_linux_musl = "${musl-cc}/bin/${musl-cc.targetPrefix}cc";
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = "${musl-cc}/bin/${musl-cc.targetPrefix}cc";
  }
)
