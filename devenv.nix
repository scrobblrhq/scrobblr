{
  pkgs,
  lib,
  config,
  ...
}:
{
  # Backend-only dev shell: Rust + Postgres/TimescaleDB + Redis + tooling.
  # (No Android/Flutter here — the mobile app lives in its own repo.)
  packages = with pkgs; [
    clang
    pkg-config
    openssl
    sqlx-cli
    biome
    just
  ];

  env = {
    OPENSSL_NO_VENDOR = "1";
    OPENSSL_DIR = "${pkgs.openssl.dev}";
    OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
    PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
  };

  # Store paths: the desktop app runs hooks outside the devenv shell.
  claude.code = {
    enable = true;
    # Replaces devenv's default, whose `prek run` checks staged files only, never the one just edited.
    hooks.git-hooks-run = {
      name = "Run git-hooks";
      hookType = "PostToolUse";
      matcher = "^(Edit|MultiEdit|Write)$";
      command = ''cd "$CLAUDE_PROJECT_DIR" && ${pkgs.jq}/bin/jq -r .tool_input.file_path | xargs ${lib.getExe config.git-hooks.package} run --files'';
    };
  };
  dotenv.enable = true;

  languages.rust = {
    enable = true;
    mold.enable = true;
  };

  services.postgres = {
    enable = true;
    listen_addresses = "localhost";
    settings.shared_preload_libraries = "timescaledb";
    initialDatabases = [ { name = "scrobblr"; } ];
    extensions = extensions: [
      extensions.timescaledb
    ];
  };

  # Applies pending migrations once Postgres is up (same as `just migrate`).
  # Offline so it compiles before the schema the queries expect exists.
  processes.migrate = {
    exec = "SQLX_OFFLINE=true cargo run -q -p worker -- migrate";
    after = [ "devenv:processes:postgres" ];
    restart.on = "never";
  };

  services.redis = {
    enable = true;
    port = 6379;
    extraConfig = "requirepass 123";
  };

  git-hooks.hooks = {
    rustfmt.enable = true;
    nixfmt.enable = true;
    biome = {
      enable = true;
      # Mirror biome.json's ignores: biome errors when every file it's given is ignored.
      excludes = [
        "^\\.sqlx/"
        "^packages/types/src/generated/"
        "^openapi\\.json$"
      ];
    };
  };
}
