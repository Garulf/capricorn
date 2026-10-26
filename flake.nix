{
  description = "Private, encrypted screen and clipboard memory for Hyprland";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      home-manager,
    }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      hmEval =
        pkgs: configType:
        (home-manager.lib.homeManagerConfiguration {
          inherit pkgs;
          modules = [
            self.homeManagerModules.default
            {
              home.username = "alice";
              home.homeDirectory = "/home/alice";
              home.stateVersion = "26.05";
              wayland.windowManager.hyprland = {
                enable = true;
                package = null;
                portalPackage = null;
                inherit configType;
              };
              programs.capricorn.enable = true;
            }
          ];
        }).config;

      hmCheck =
        pkgs: configType: file: pattern:
        let
          cfg = hmEval pkgs configType;
        in
        pkgs.runCommand "capricorn-hm-${configType}" { } ''
          grep -F -- ${nixpkgs.lib.escapeShellArg pattern} ${cfg.xdg.configFile.${file}.source}
          grep -F -- 'capricorn show search' ${cfg.xdg.configFile.${file}.source}
          test -s ${cfg.xdg.configFile."capricorn/capture.toml".source}
          touch $out
        '';
    in
    {
      packages = forAllSystems (pkgs: {
        default = pkgs.callPackage ./nix/package.nix { };
      });

      nixosModules.default = import ./nix/nixos-module.nix self;
      homeManagerModules.default = import ./nix/hm-module.nix self;

      checks = forAllSystems (
        pkgs:
        {
          package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
          hm-lua = hmCheck pkgs "lua" "hypr/capricorn.lua" "\"screencopy\", \"allow\"";
          hm-hyprlang = hmCheck pkgs "hyprlang" "hypr/hyprland.conf" "screencopy, allow";
        }
        // nixpkgs.lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == "x86_64-linux") {
          vm = pkgs.testers.runNixOSTest (import ./nix/tests/vm.nix self);
        }
      );

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.default ];
          packages = with pkgs; [
            cargo
            clippy
            rustfmt
            rust-analyzer
            pre-commit
          ];
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (
            with pkgs;
            [
              libxkbcommon
              wayland
              vulkan-loader
              libGL
            ]
          );
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
