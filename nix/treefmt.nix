{inputs, ...}: {
  imports = [inputs.treefmt-nix.flakeModule];

  perSystem = {
    config,
    rustToolchain,
    ...
  }: {
    treefmt = {
      programs.alejandra.enable = true;
      programs.rustfmt = {
        enable = true;
        package = rustToolchain;
      };
    };

    formatter = config.treefmt.build.wrapper;
  };
}
