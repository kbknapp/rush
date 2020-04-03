# Run like this:
#   nix-build /path/to/this/directory
# ... build products will be in ./result

{ pkgs ? (import <nixpkgs> {}), source ? ./., version ? "dev" }:

with pkgs;

stdenv.mkDerivation rec {
  name = "rush-${version}";
  #src = lib.cleanSource (lib.sourceByRegex source ["target/*"]);

  buildInputs = [ rustc cargo ];
  inherit version;

  # ...

}
