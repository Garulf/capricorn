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
      url = "https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.onnx";
      hash = "sha256-qReyPb2VJLRl336SJkGz/ymBYj303tWgI0AE7y/ufP4=";
    };
    recognition = fetchurl {
      url = "https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.onnx";
      hash = "sha256-hsFFwu25bIyu1bHruPRNcGQIkiIRRRwwnGJRV91gYcU=";
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
  version = "1.0.0"; # x-release-please-version

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
    install -Dm444 ${ocrModels.detection} $out/share/capricorn/models/text-detection.onnx
    install -Dm444 ${ocrModels.recognition} $out/share/capricorn/models/text-recognition.onnx
  '';

  # The UI dlopens Wayland, xkbcommon and GPU libraries. Capture dlopens
  # Wayland too: Cargo unifies features across the workspace build, so it gets
  # winit's dlopen backend. An rpath keeps each binary at its own store path
  # instead of behind a wrapper script.
  postFixup = ''
    patchelf --add-rpath ${lib.makeLibraryPath uiRuntimeLibs} $out/bin/capricorn-ui
    patchelf --add-rpath ${lib.makeLibraryPath [ wayland ]} $out/bin/capricorn-capture
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
