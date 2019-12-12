let
  pkgs = import <nixpkgs> {};
  lorri = import ./default.nix {};
  lorriBin = if true then ./target/debug/lorri else "${lorri}/bin/lorri";

   script = pkgs.writeShellScript "lol" ''
     started=$(date '+%s')
     while sleep 5; do
       printf "This service has been up %s seconds\n" $(($(date '+%s') - started))
     done
   '';

   check = pkgs.writeShellScript "check" ''
     while sleep 5; do
       cargo fmt && cargo check && cargo clippy && cargo build
       printf "This service has been up %s seconds\n" $(($(date '+%s') - started))
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
        name = "the-time-is";
        program = script;
        args = [];
      }
      {
        name = "check";
        program = check;
        args = [];
      }
    ];
  }
)
