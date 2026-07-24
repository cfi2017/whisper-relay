self:
{ config, lib, pkgs, ... }:

let
  cfg = config.programs.whisper-relay;
  tomlFormat = pkgs.formats.toml { };
  runtimePackages = with pkgs; [
    pipewire
    gst_all_1.gstreamer
    gst_all_1.gst-plugins-base
    gst_all_1.gst-plugins-good
    gst_all_1.gst-plugins-bad
  ];
in
{
  options.programs.whisper-relay = {
    enable = lib.mkEnableOption "Whisper Relay client";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.client;
      defaultText = lib.literalExpression "inputs.whisper-relay.packages.\${pkgs.stdenv.hostPlatform.system}.client";
      description = "Whisper Relay client package to install.";
    };

    settings = lib.mkOption {
      type = tomlFormat.type;
      default = { };
      example = {
        server_url = "wss://whisper.example.com/v1/sessions/ws";
        output = "~/Documents/meetings/transcript.md";
        oidc_issuer = "https://issuer.example.com";
        oidc_client_id = "whisper-relay-device-client";
        diarization = "prefer";
        chunk_seconds = 15;
        source = [ "42" "84" ];
      };
      description = "Settings written to xdg.configFile `whisper-relay/client.toml`.";
    };

    installRuntimeDependencies = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Install PipeWire and GStreamer command-line/runtime dependencies used by live capture.";
    };

    extraPackages = lib.mkOption {
      type = lib.types.listOf lib.types.package;
      default = [ ];
      description = "Additional packages to install with the client.";
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages =
      [ cfg.package ]
      ++ lib.optionals cfg.installRuntimeDependencies runtimePackages
      ++ cfg.extraPackages;

    xdg.configFile."whisper-relay/client.toml".source =
      tomlFormat.generate "whisper-relay-client.toml" cfg.settings;
  };
}

