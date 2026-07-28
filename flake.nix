{
  description = "Whisper Relay client/server workspace";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
        workspaceVersion = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
        commonBuildInputs = with pkgs; [
          openssl
          pkg-config
        ];
        clientRuntimeInputs = with pkgs; [
          coreutils
          pipewire
          gst_all_1.gstreamer
          gst_all_1.gst-plugins-base
          gst_all_1.gst-plugins-good
          gst_all_1.gst-plugins-bad
        ];
        gstPluginPath = pkgs.lib.makeSearchPath "lib/gstreamer-1.0" clientRuntimeInputs;
      in
      {
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            clippy
            rustc
            rustfmt
          ] ++ commonBuildInputs ++ clientRuntimeInputs;
          RUST_BACKTRACE = "1";
        };

        packages.client = pkgs.rustPlatform.buildRustPackage {
          pname = "whisper-relay-client";
          version = workspaceVersion;
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          buildAndTestSubdir = ".";
          cargoBuildFlags = [ "-p" "whisper-relay-client" ];
          nativeBuildInputs = commonBuildInputs ++ [ pkgs.makeWrapper ];
          buildInputs = commonBuildInputs;
          postInstall = ''
            wrapProgram $out/bin/whisper-relay-client \
              --prefix PATH : ${pkgs.lib.makeBinPath clientRuntimeInputs} \
              --set GST_PLUGIN_SYSTEM_PATH_1_0 ${gstPluginPath}
          '';
        };

        packages.server = pkgs.rustPlatform.buildRustPackage {
          pname = "whisper-relay-server";
          version = workspaceVersion;
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          buildAndTestSubdir = ".";
          cargoBuildFlags = [ "-p" "whisper-relay-server" ];
          nativeBuildInputs = commonBuildInputs;
          buildInputs = commonBuildInputs;
        };
      }) // {
        homeManagerModules.default = import ./nix/home-manager.nix self;
        homeManagerModules.whisper-relay-client = import ./nix/home-manager.nix self;
      };
}
