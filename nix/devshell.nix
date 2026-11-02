{
  perSystem = {
    pkgs,
    rustToolchain,
    ...
  }: {
    devShells.default = pkgs.mkShell {
      packages = [rustToolchain pkgs.kubectl pkgs.stalwart-cli];
    };
  };
}
