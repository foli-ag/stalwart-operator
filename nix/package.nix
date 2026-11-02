{...}: {
  perSystem = {
    craneLib,
    pkgs,
    ...
  }: let
    commonArgs = {
      src = pkgs.lib.cleanSourceWith {
        src = ../.;
        # `crd.yaml` is read by the test that keeps it in step with the code.
        filter = path: type:
          (craneLib.filterCargoSources path type)
          || pkgs.lib.hasSuffix "/deploy/crd.yaml" path;
      };
      strictDeps = true;
    };
    cargoArtifacts = craneLib.buildDepsOnly commonArgs;
    stalwart-operator = craneLib.buildPackage (commonArgs // {inherit cargoArtifacts;});
    image = pkgs.dockerTools.buildLayeredImage {
      name = "stalwart-operator";
      tag = stalwart-operator.version;
      contents = [pkgs.cacert];
      config = {
        Cmd = ["${stalwart-operator}/bin/stalwart-operator" "run"];
        Env = ["SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"];
        User = "65532:65532";
      };
    };
  in {
    packages = {
      inherit stalwart-operator image;
      default = stalwart-operator;
    };
    checks = {inherit stalwart-operator;};
  };
}
