# NixOS has no default containers policy.
{...}: {
  perSystem = {pkgs, ...}: let
    skopeo = pkgs.runCommand "skopeo" {nativeBuildInputs = [pkgs.makeWrapper];} ''
      makeWrapper ${pkgs.skopeo}/bin/skopeo $out/bin/skopeo \
        --add-flags "--policy ${pkgs.writeText "policy.json" (builtins.toJSON {
        default = [{type = "insecureAcceptAnything";}];
      })}"
    '';
  in {
    packages.skopeo = skopeo;
    apps.skopeo = {
      type = "app";
      program = "${skopeo}/bin/skopeo";
    };
  };
}
