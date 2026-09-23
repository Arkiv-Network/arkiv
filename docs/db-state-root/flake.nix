{
  description = "The Arkiv database state-root deck: diagrams (Graphviz) and slides (Marp).";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      tools = pkgs: [ pkgs.python3 pkgs.graphviz pkgs.marp-cli ];
      devTools = pkgs: tools pkgs ++ [ pkgs.watchexec ];
    in {
      packages = forAll (pkgs: {
        default = pkgs.stdenvNoCC.mkDerivation {
          pname = "arkiv-db-state-root-deck";
          version = "0.1.0";
          src = ./.;
          nativeBuildInputs = tools pkgs;
          buildPhase = ''
            export HOME=$TMPDIR
            python3 gen_diagrams.py diagrams
            for f in diagrams/*.dot; do
              dot -Tsvg "$f" -o "''${f%.dot}.svg"
            done
            marp --html slides.md -o slides.html
          '';
          installPhase = ''
            mkdir -p $out/diagrams
            cp slides.html slides.md $out/
            cp diagrams/*.svg diagrams/*.dot $out/diagrams/
          '';
        };
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell { packages = devTools pkgs; };
      });
    };
}
