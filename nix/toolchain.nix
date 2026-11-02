{inputs, ...}: {
  perSystem = {system, ...}: let
    pkgs = import inputs.nixpkgs {
      inherit system;
      overlays = [inputs.rust-overlay.overlays.default];
    };
    # Pinned: `stable.latest` moves with every rust-overlay bump and would
    # invalidate every cached build in CI.
    rustVersion = "1.96.0";
    rustToolchain = pkgs.rust-bin.stable.${rustVersion}.default.override {
      extensions = ["rust-src" "clippy" "rustfmt" "rust-analyzer"];
    };
    # The package only needs rustc, cargo and std.
    craneLib = (inputs.crane.mkLib pkgs).overrideToolchain pkgs.rust-bin.stable.${rustVersion}.minimal;
  in {
    _module.args = {inherit pkgs rustToolchain craneLib;};
  };
}
