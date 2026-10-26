self:
{
  config,
  lib,
  pkgs,
  options,
  ...
}:

let
  cfg = config.programs.capricorn;
  toml = pkgs.formats.toml { };
  hyprland = config.wayland.windowManager.hyprland;

  captureBinary = "${cfg.package}/bin/capricorn-capture";
  cli = "${cfg.package}/bin/capricorn";

  captureConfig = toml.generate "capture.toml" (
    {
      floor_secs = cfg.capture.floorSecs;
      settle_ms = cfg.capture.settleMs;
      idle_secs = cfg.capture.idleSecs;
      pause_fullscreen_classes = cfg.pauseFullscreenClasses;
    }
    // lib.optionalAttrs (cfg.exclude != [ ]) {
      exclude = map (lib.filterAttrs (_: v: v != null)) cfg.exclude;
    }
  );

  actions = {
    search = "${cli} show search";
    timeline = "${cli} show timeline";
    lock = "${cli} lock";
    pause = "${cli} pause";
    forget = "${cli} forget";
  };
  boundActions = lib.filterAttrs (name: _: cfg.keybinds.${name} != null) actions;

  luaString = s: builtins.toJSON s;

  luaConfig = ''
    hl.config({ ecosystem = { enforce_permissions = true } })
    hl.permission(${luaString "^${lib.escapeRegex captureBinary}$"}, "screencopy", "allow")
  ''
  + lib.concatStrings (
    lib.mapAttrsToList (
      name: command:
      "hl.bind(${luaString cfg.keybinds.${name}}, hl.dsp.exec_cmd(${luaString command}))\n"
    ) boundActions
  );

  hyprlangKeys =
    combo:
    let
      parts = map lib.trim (lib.splitString "+" combo);
    in
    "${lib.concatStringsSep " " (lib.init parts)}, ${lib.last parts}";

  hyprlangConfig = ''
    ecosystem {
      enforce_permissions = true
    }
    permission = ^${lib.escapeRegex captureBinary}$, screencopy, allow
  ''
  + lib.concatStrings (
    lib.mapAttrsToList (
      name: command: "bind = ${hyprlangKeys cfg.keybinds.${name}}, exec, ${command}\n"
    ) boundActions
  );

  hasConfigType = options.wayland.windowManager.hyprland ? configType;
  usesLua = hasConfigType && hyprland.configType == "lua";

  excludeRule = lib.types.submodule {
    options = {
      class = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Case-insensitive regex matched against the window class.";
      };
      title = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Case-insensitive regex matched against the window title.";
      };
      workspace = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Exact workspace name.";
      };
    };
  };

  keybind =
    default:
    lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      inherit default;
      description = "Hyprland key combo such as `SUPER + ALT + R`, or `null` for no bind.";
    };

  userService = description: exec: {
    Unit = {
      Description = description;
      PartOf = [ "graphical-session.target" ];
      After = [ "graphical-session.target" ];
    };
    Service = {
      ExecStart = exec;
      Restart = "on-failure";
      RestartSec = 2;
    };
    Install.WantedBy = [ "graphical-session.target" ];
  };
in
{
  options.programs.capricorn = {
    enable = lib.mkEnableOption "Capricorn screen capture, search overlay and timeline";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "capricorn.packages.\${system}.default";
      description = "Capricorn package. Must match the one used by services.capricorn.";
    };

    exclude = lib.mkOption {
      type = lib.types.listOf excludeRule;
      default = [ ];
      example = [
        { class = "^signal$"; }
        { title = "bank"; }
        { workspace = "9"; }
      ];
      description = ''
        Windows that are never recorded, added to the built-in list of
        password managers and private browser windows. Fields in one rule
        must all match.
      '';
    };

    pauseFullscreenClasses = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [
        "steam_app_.*"
        "gamescope"
        "mpv"
      ];
      description = "Recording pauses while a fullscreen window's class fully matches one of these regexes.";
    };

    capture = {
      floorSecs = lib.mkOption {
        type = lib.types.ints.positive;
        default = 5;
        description = "Capture at least this often while you are active.";
      };
      settleMs = lib.mkOption {
        type = lib.types.ints.positive;
        default = 1500;
        description = "Capture after this long without input.";
      };
      idleSecs = lib.mkOption {
        type = lib.types.ints.positive;
        default = 300;
        description = "Stop capturing after this long without input.";
      };
    };

    keybinds = {
      search = keybind "SUPER + ALT + R";
      timeline = keybind "SUPER + ALT + T";
      lock = keybind "SUPER + ALT + L";
      pause = keybind "SUPER + ALT + P";
      forget = keybind null;
    };

    hyprland.enable = lib.mkOption {
      type = lib.types.bool;
      default = hyprland.enable;
      defaultText = lib.literalExpression "config.wayland.windowManager.hyprland.enable";
      description = ''
        Add the screencopy permission and keybinds to the Home Manager
        managed Hyprland config. Emits Lua when
        `wayland.windowManager.hyprland.configType` is `"lua"`.
      '';
    };
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
        home.packages = [ cfg.package ];
        xdg.configFile."capricorn/capture.toml".source = captureConfig;

        systemd.user.services.capricorn-capture = lib.recursiveUpdate (userService "Capricorn screen and clipboard capture" captureBinary) {
          Unit.X-Restart-Triggers = [ "${captureConfig}" ];
        };
        systemd.user.services.capricorn-ui =
          userService "Capricorn search overlay and timeline" "${cfg.package}/bin/capricorn-ui";
      }
      (lib.mkIf (cfg.hyprland.enable && usesLua) {
        wayland.windowManager.hyprland.extraLuaFiles.capricorn = luaConfig;
      })
      (lib.mkIf (cfg.hyprland.enable && !usesLua) {
        wayland.windowManager.hyprland.extraConfig = hyprlangConfig;
      })
    ]
  );
}
