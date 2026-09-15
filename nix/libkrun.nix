# The nixpkgs pin owns the release and source hashes. These fixes preserve exact
# launch configuration and count the bundled kernel inside the requested RAM.
{pkgs}: pkgs.libkrun.overrideAttrs (old: {
  patches = (old.patches or []) ++ [
    ./libkrun-exact-config.patch
    ./libkrun-memory.patch
  ];
  postBuild = (old.postBuild or "") + ''
    cargo test --release --offline -p krun-arch --lib embedded_kernel_stays_within_configured_ram
    cargo test --release --offline -p krun-vmm --lib embedded_kernel_outside_ram_returns_error
  '';
  preBuild = (old.preBuild or "") + ''
    # Exercise the patched JSON parser before its init binary is embedded.
    $CC -O1 -g -fsanitize=address,undefined -Iinit init/config_test.c init/dhcp.c -o config-test
    ASAN_OPTIONS=detect_leaks=1 ./config-test
  '';
})
