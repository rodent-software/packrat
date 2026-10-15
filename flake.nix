{
  description = "A free and open source physical media backup utility";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);

      mkPackrat = system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = "packrat";
          version = "0.1.0-rc.2";
          src = self;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.makeWrapper pkgs.pkg-config ];
          # libdvdcss is supplied by nixpkgs, never bundled. Point the runtime
          # loader at it on Linux; macOS users can set PACKRAT_DVDCSS. The rest
          # is the windowing and graphics stack the Iced window needs.
          buildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [
            pkgs.libdvdcss
            pkgs.libxkbcommon
            pkgs.vulkan-loader
            pkgs.wayland
            pkgs.libX11
          ];
          postInstall = pkgs.lib.optionalString pkgs.stdenv.isLinux ''
            wrapProgram "$out/bin/packrat" \
              --prefix PACKRAT_DVDCSS : "${pkgs.libdvdcss}/lib" \
              --prefix LD_LIBRARY_PATH : "${pkgs.lib.makeLibraryPath [
                pkgs.vulkan-loader
                pkgs.libxkbcommon
                pkgs.wayland
                pkgs.libX11
              ]}"
          '';

          meta = with pkgs.lib; {
            description = "A free and open source physical media backup utility";
            homepage = "https://github.com/rodent-software/packrat";
            license = licenses.gpl3Plus;
            mainProgram = "packrat";
            platforms = platforms.unix;
          };
        };
    in
    {
      packages = forAllSystems (system: {
        default = mkPackrat system;
        packrat = mkPackrat system;
      });

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/packrat";
        };
      });

      devShells = forAllSystems (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.rustfmt
              pkgs.clippy
              pkgs.pkg-config
            ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
              pkgs.libdvdcss
              pkgs.libxkbcommon
              pkgs.vulkan-loader
              pkgs.wayland
              pkgs.libX11
            ];
          };
        });
    };
}
