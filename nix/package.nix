{
  lib,
  rustPlatform,
  fetchurl,
  pkg-config,
  patchelf,
  libxkbcommon,
  wayland,
  vulkan-loader,
  libGL,
}:

let
  ocrModels = {
    detection = fetchurl {
      url = "https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.rten";
      hash = "sha256-8Vz7Vr0CxL9HiiA0OYZQSh8B4WZcKzoK1mNA8FSxtco=";
    };
    recognition = fetchurl {
      url = "https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.rten";
      hash = "sha256-5ISGbUzOQDF1vY0AsSj+sIq0LiCN4w5CzZiJ2PFzWm4=";
    };
  };
  uiRuntimeLibs = [
    libxkbcommon
    wayland
    vulkan-loader
    libGL
  ];
in
rustPlatform.buildRustPackage {
  pname = "capricorn";
  version = "0.1.0"; # x-release-please-version

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    patchelf
  ];
  buildInputs = [ libxkbcommon ];

  # Tests run in CI with `cargo test`; the sandboxed build only compiles.
  doCheck = false;

  postInstall = ''
    install -Dm444 ${ocrModels.detection} $out/share/capricorn/models/text-detection.rten
    install -Dm444 ${ocrModels.recognition} $out/share/capricorn/models/text-recognition.rten
  '';

  # The UI dlopens Wayland, xkbcommon and GPU libraries. An rpath keeps the
  # binary at its own store path, which the daemon checks as a peer identity.
  postFixup = ''
    patchelf --add-rpath ${lib.makeLibraryPath uiRuntimeLibs} $out/bin/capricorn-ui
  '';

  passthru = { inherit ocrModels; };

  meta = {
    description = "Private, encrypted screen and clipboard memory for Hyprland";
    homepage = "https://github.com/Garulf/capricorn";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = "capricorn";
  };
}
