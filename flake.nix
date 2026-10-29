{
  description = "acb-bridge: 多 harness × 多聊天平台的桥";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs";
  };

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
    in
    {
      packages.${system} = rec {
        default = acb-bridge;
        acb-bridge = pkgs.rustPlatform.buildRustPackage {
          pname = "acb-bridge";
          version = "0.1.0";
          src = ./.;

          # libgit2 通过 git2 crate 的 vendored 实现编译，需要这些系统依赖。
          nativeBuildInputs = with pkgs; [
            pkg-config
          ];
          buildInputs = with pkgs; [
            openssl
            zlib
            libgit2
          ];

          cargoLock = {
            lockFile = ./Cargo.lock;
          };
        };
      };

      devShells.${system}.default = pkgs.mkShell {
        nativeBuildInputs = with pkgs; [
          cargo
          rustc
          pkg-config
        ];
        buildInputs = with pkgs; [
          openssl
          zlib
          libgit2
        ];
        # 让 cargo 在这个 dev shell 里能正常解包（nix 提供干净的文件系统）。
        shellHook = ''
          export CARGO_HOME="''${CARGO_HOME:-$HOME/.cargo}"
        '';
      };
    };
}
