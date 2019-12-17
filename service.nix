let
  pkgs = import <nixpkgs> {};
  lorri = import ./default.nix {};
  lorriBin = if true then ./target/debug/lorri else "${lorri}/bin/lorri";

  check = pkgs.writeShellScript "check" ''
     while sleep 5; do
       cargo fmt && cargo check && cargo clippy && cargo build
     done
   '';

in
pkgs.writeText "services.json" (
  builtins.toJSON {
    services = [
      {
        name = "lorri";
        program = lorriBin;
        args = [ "daemon" ];
      }
      {
        name = "check";
        program = check;
        args = [];
      }
    ];
  }
)
