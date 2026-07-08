{
  description = "multimedia-rpc toolchain: rust, ffmpeg, ImageMagick, GStreamer";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };
      gst = pkgs.gst_all_1;
      rust = pkgs.rust-bin.stable."1.97.1".default.override {
        extensions = [ "clippy" "rustfmt" "llvm-tools-preview" ];
      };
    in {
      devShells.${system}.default = pkgs.mkShell {
        name = "multimedia-rpc";

        nativeBuildInputs = [
          rust
          pkgs.stdenv.cc
          pkgs.pkg-config
          pkgs.protobuf
        ];

        buildInputs = [
          pkgs.ffmpeg
          pkgs.imagemagick
          pkgs.glib
          gst.gstreamer
          gst.gst-plugins-base
          gst.gst-plugins-good
          gst.gst-libav
        ];

        packages = [
          pkgs.nodejs
          pkgs.buf
          pkgs.cargo-llvm-cov
          pkgs.sqlx-cli
        ];
      };
    };
}