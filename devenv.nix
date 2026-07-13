{ pkgs, lib, ... }: {
  # Backend-only dev shell: Rust + Postgres/TimescaleDB + Redis + tooling.
  # (No Android/Flutter here — the mobile app lives in its own repo.)
  packages = with pkgs; [
    clang
    pkg-config
    openssl
    sqlx-cli
    turbo
    biome
    just
  ];

  env = {
    OPENSSL_NO_VENDOR = "1";
    OPENSSL_DIR = "${pkgs.openssl.dev}";
    OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
    PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
  };

  claude.code.enable = true;
  dotenv.enable = true;

  languages.rust = {
    enable = true;
    mold.enable = true;
  };

  services.postgres = {
    enable = true;
    listen_addresses = "localhost";
    settings.shared_preload_libraries = "timescaledb";
    initialDatabases = [
      {
        name = "scrobblr";
        schema = ./migrations/0001_initial.sql;
      }
    ];
    extensions = extensions: [
      extensions.timescaledb
    ];
  };

  services.redis = {
    enable = true;
    port = 6379;
    extraConfig = "requirepass 123";
  };

  git-hooks.hooks = {
    rustfmt.enable = true;
    nixfmt.enable = true;
    biome.enable = true;
  };
}
