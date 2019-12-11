let
  pkgs = import <nixpkgs> {};
in
pkgs.writeText "services.json" (
  builtins.toJSON {
    services = [
      {
        name = "httpd";
        program = "${pkgs.python3}/bin/python3";
        args = [ "-m" "http.server" "8000" "--bind" "127.0.0.1" "--directory" ./serve ];
      }
    ];
  }
)
