{
  description = "bazel-mcp development shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { nixpkgs, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let pkgs = nixpkgs.legacyPackages.${system};
      in {
        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.bazelisk
            pkgs.cargo-fuzz
            pkgs.cargo-shear
            pkgs.git
            pkgs.hyperfine
            pkgs.jq
            pkgs.python3
          ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.clang ];
        };
      });
}
