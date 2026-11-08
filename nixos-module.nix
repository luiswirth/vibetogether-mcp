# The server as a NixOS service. It acts for no one, so it runs as a user of
# its own, which keeps nothing on disk. It listens on 127.0.0.1 only, and the
# host puts a front before it, such as a reverse proxy.
self: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.vibe-together-mcp;
in {
  options.services.vibe-together-mcp = {
    enable = lib.mkEnableOption "the Vibe Together MCP server";
    port = lib.mkOption {
      type = lib.types.port;
      default = 8792;
      description = "The port the server listens on, on 127.0.0.1.";
    };
    hosts = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = ["localhost" "127.0.0.1" "::1"];
      example = ["vibe-together.agentics.lwirth.com"];
      description = "The hosts the server accepts requests for, such as the domain it is served at.";
    };
    relay = lib.mkOption {
      type = lib.types.str;
      default = "https://agentwire.agentics.lwirth.com";
      description = "The agentwire relay a session token alone refers to.";
    };
  };

  config.systemd.services.vibe-together-mcp = lib.mkIf cfg.enable {
    description = "The Vibe Together MCP server";
    after = ["network-online.target"];
    wants = ["network-online.target"];
    wantedBy = ["multi-user.target"];
    environment = {
      VIBE_TOGETHER_MCP_ADDRESS = "127.0.0.1:${toString cfg.port}";
      VIBE_TOGETHER_MCP_HOSTS = lib.concatStringsSep "," cfg.hosts;
      AGENTWIRE_RELAY = cfg.relay;
    };
    serviceConfig = {
      ExecStart = lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      Restart = "on-failure";
      DynamicUser = true;
      ProtectHome = true;
      PrivateDevices = true;
      ProtectClock = true;
      ProtectControlGroups = true;
      ProtectHostname = true;
      ProtectKernelLogs = true;
      ProtectKernelModules = true;
      ProtectKernelTunables = true;
      ProtectProc = "invisible";
      CapabilityBoundingSet = "";
      LockPersonality = true;
      MemoryDenyWriteExecute = true;
      # AF_UNIX for looking up the relay's name through nscd.
      RestrictAddressFamilies = ["AF_INET" "AF_INET6" "AF_UNIX"];
      RestrictNamespaces = true;
      RestrictRealtime = true;
      SystemCallArchitectures = "native";
      SystemCallFilter = ["@system-service" "~@privileged"];
      UMask = "0077";
    };
  };
}
