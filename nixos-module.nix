# The server as a NixOS service. It acts for no one, so it runs as a user of
# its own, which keeps nothing on disk. It listens on 127.0.0.1 only, and the
# host puts a front before it, such as a reverse proxy.
self: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.services.vibetogether-mcp;
in {
  options.services.vibetogether-mcp = {
    enable = lib.mkEnableOption "the Vibe Together MCP server";
    port = lib.mkOption {
      type = lib.types.port;
      default = 8792;
      description = "The port the server listens on, on 127.0.0.1.";
    };
    hosts = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = ["localhost" "127.0.0.1" "::1"];
      example = ["vibetogether.agentics.lwirth.com"];
      description = "The hosts the server accepts requests for, such as the domain it is served at.";
    };
    relay = lib.mkOption {
      type = lib.types.str;
      default = "https://agentwire.agentics.lwirth.com";
      description = "The agentwire relay a session token alone refers to.";
    };
    accounts = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "http://127.0.0.1:8793";
      description = "The account service, under which `/api/me` answers, which joining a room signs in to; null for none.";
    };
    resource = lib.mkOption {
      type = lib.types.str;
      default = "https://vibetogether.agentics.lwirth.com/mcp";
      description = "The server's public URL, by which its 401 challenge names its metadata.";
    };
  };

  config.systemd.services.vibetogether-mcp = lib.mkIf cfg.enable {
    description = "The Vibe Together MCP server";
    after = ["network-online.target"];
    wants = ["network-online.target"];
    wantedBy = ["multi-user.target"];
    environment =
      {
        VIBETOGETHER_MCP_ADDRESS = "127.0.0.1:${toString cfg.port}";
        VIBETOGETHER_MCP_HOSTS = lib.concatStringsSep "," cfg.hosts;
        AGENTWIRE_RELAY = cfg.relay;
      }
      // lib.optionalAttrs (cfg.accounts != null) {
        VIBETOGETHER_MCP_ACCOUNTS = cfg.accounts;
        VIBETOGETHER_MCP_RESOURCE = cfg.resource;
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
