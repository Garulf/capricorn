self:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.capricorn;
  toml = pkgs.formats.toml { };

  userUid =
    name:
    let
      user = config.users.users.${name};
    in
    if user.uid != null then
      user.uid
    else
      throw "services.capricorn.users: user '${name}' needs an explicit uid";

  daemonSettings = lib.recursiveUpdate {
    lock_timeout_secs = cfg.lockTimeout;
    retention_days = cfg.retention.days;
    retention_max_gib = cfg.retention.maxGiB;
    clipboard_retention_days = cfg.retention.clipboardDays;
    # The daemon runs as its own uid and can't read other users'
    # /proc/<pid>/exe, so peers are checked by uid and socket group only.
    allowed_uids = map userUid cfg.users;
  } cfg.settings;

  configFile = toml.generate "capricorn.toml" (lib.filterAttrsRecursive (_: v: v != null) daemonSettings);

  swapIsSafe = lib.all (
    swap: swap.randomEncryption.enable or false || lib.hasPrefix "/dev/mapper/" (swap.device or "")
  ) config.swapDevices;
  coredumpsOff = (config.systemd.coredump.settings.Coredump.Storage or null) == "none";
  hibernationSafe =
    config.boot.resumeDevice == "" || lib.hasPrefix "/dev/mapper/" config.boot.resumeDevice;
in
{
  options.services.capricorn = {
    enable = lib.mkEnableOption "the Capricorn vault daemon";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "capricorn.packages.\${system}.default";
      description = "Capricorn package.";
    };

    users = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "alice" ];
      description = ''
        Users allowed to talk to the daemon. They are added to the
        `capricorn` group, which owns the sockets. Each needs an explicit uid.
      '';
    };

    lockTimeout = lib.mkOption {
      type = lib.types.ints.positive;
      default = 900;
      description = "Seconds of inactivity after which the vault locks.";
    };

    retention = {
      days = lib.mkOption {
        type = lib.types.nullOr lib.types.ints.positive;
        default = 90;
        description = "Delete history older than this many days. `null` keeps it forever.";
      };
      maxGiB = lib.mkOption {
        type = lib.types.nullOr lib.types.ints.positive;
        default = 50;
        description = "Delete the oldest history once the vault exceeds this size. `null` disables the cap.";
      };
      clipboardDays = lib.mkOption {
        type = lib.types.nullOr lib.types.ints.positive;
        default = null;
        description = "Separate, usually shorter, retention for clipboard entries.";
      };
    };

    settings = lib.mkOption {
      inherit (toml) type;
      default = { };
      description = "Extra daemon settings merged into /etc/capricorn/config.toml.";
    };

    allowInsecureHost = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Skip the checks that plaintext held in memory can't reach disk through
        unencrypted swap, hibernation images or core dumps.
      '';
    };

    ptraceScope = lib.mkOption {
      type = lib.types.ints.between 0 3;
      default = 1;
      description = "Value for kernel.yama.ptrace_scope.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = lib.optionals (!cfg.allowInsecureHost) [
      {
        assertion = swapIsSafe;
        message = "services.capricorn: every swap device must use randomEncryption or live on LUKS (or set services.capricorn.allowInsecureHost).";
      }
      {
        assertion = hibernationSafe;
        message = "services.capricorn: hibernation must be disabled or resume from an encrypted device (or set services.capricorn.allowInsecureHost).";
      }
      {
        assertion = coredumpsOff;
        message = "services.capricorn: systemd.coredump.settings.Coredump.Storage must be \"none\" (or set services.capricorn.allowInsecureHost).";
      }
    ];

    warnings = lib.optional (
      config.boot.initrd.luks.devices == { }
    ) "services.capricorn: no LUKS devices configured; full-disk encryption is strongly recommended.";

    systemd.coredump.settings.Coredump.Storage = lib.mkDefault "none";
    boot.kernel.sysctl."kernel.yama.ptrace_scope" = lib.mkDefault cfg.ptraceScope;

    users.groups.capricorn = { };
    users.users = lib.mkMerge [
      {
        capricorn = {
          isSystemUser = true;
          group = "capricorn";
          home = "/var/lib/capricorn";
        };
      }
      (lib.genAttrs cfg.users (_: {
        extraGroups = [ "capricorn" ];
      }))
    ];

    environment.etc."capricorn/config.toml".source = configFile;
    environment.systemPackages = [ cfg.package ];

    systemd.services.capricornd = {
      description = "Capricorn vault daemon";
      wantedBy = [ "multi-user.target" ];
      after = [ "dbus.service" ];
      restartTriggers = [ configFile ];
      environment = {
        CAPRICORN_CONFIG = "/etc/capricorn/config.toml";
        CAPRICORN_DATA_DIR = "/var/lib/capricorn";
        CAPRICORN_RUNTIME_DIR = "/run/capricorn";
        CAPRICORN_OCR_MODELS = "${cfg.package}/share/capricorn/models";
        RUST_LOG = "info";
      };
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/capricornd";
        User = "capricorn";
        Group = "capricorn";
        Restart = "on-failure";
        StateDirectory = "capricorn";
        StateDirectoryMode = "0700";
        RuntimeDirectory = "capricorn";
        RuntimeDirectoryMode = "0750";
        UMask = "0007";
        LimitCORE = 0;
        LimitMEMLOCK = "64M";
        PrivateNetwork = true;
        RestrictAddressFamilies = [ "AF_UNIX" ];
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectProc = "default";
        LockPersonality = true;
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        MemoryDenyWriteExecute = true;
        SystemCallArchitectures = "native";
        SystemCallFilter = [
          "@system-service"
          "~@privileged"
        ];
        CapabilityBoundingSet = "";
      };
    };
  };
}
