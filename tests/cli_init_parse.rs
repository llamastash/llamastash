//! Integration coverage for the `init` clap surface — repeatable +
//! comma-separated `--only`/`--skip`, mutual exclusion, the
//! `--yes --json --offline` triple, and the new exit codes.

use std::path::PathBuf;

use clap::Parser;
use llamastash::cli::cli_args::{
  Cli, Command, ConfigOverride, InitStep, InstallOverride, ModelOverride,
};
use llamastash::cli::exit_codes::{
  INIT_ABORTED, INIT_DOWNLOAD_FAILED, INIT_SMOKE_FAILED, PULL_FAILED, UNKNOWN,
};

fn parse(argv: &[&str]) -> Cli {
  Cli::try_parse_from(std::iter::once("llamastash").chain(argv.iter().copied()))
    .expect("argv should parse")
}

#[test]
fn init_only_server_alone() {
  match parse(&["init", "--only", "server"]).command {
    Some(Command::Init(args)) => assert_eq!(args.only, vec![InitStep::Server]),
    other => panic!("expected init, got {other:?}"),
  }
}

#[test]
fn init_only_comma_separated_server_and_config() {
  match parse(&["init", "--only", "server,config"]).command {
    Some(Command::Init(args)) => {
      assert_eq!(args.only, vec![InitStep::Server, InitStep::Config]);
    }
    other => panic!("expected init, got {other:?}"),
  }
}

#[test]
fn init_only_repeatable_flag() {
  match parse(&["init", "--only", "server", "--only", "models"]).command {
    Some(Command::Init(args)) => {
      assert_eq!(args.only, vec![InitStep::Server, InitStep::Models]);
    }
    other => panic!("expected init, got {other:?}"),
  }
}

#[test]
fn init_skip_repeatable_and_comma_separated() {
  match parse(&["init", "--skip", "models,config"]).command {
    Some(Command::Init(args)) => {
      assert_eq!(args.skip, vec![InitStep::Models, InitStep::Config]);
      assert!(args.only.is_empty());
    }
    other => panic!("expected init, got {other:?}"),
  }
}

#[test]
fn init_only_and_skip_conflict() {
  let result = Cli::try_parse_from(["llamastash", "init", "--only", "server", "--skip", "config"]);
  assert!(result.is_err(), "--only and --skip must conflict");
}

#[test]
fn init_yes_json_offline_combinable() {
  match parse(&["init", "--yes", "--json", "--offline"]).command {
    Some(Command::Init(args)) => {
      assert!(args.yes);
      assert!(args.json);
      assert!(args.offline);
    }
    other => panic!("expected init, got {other:?}"),
  }
}

#[test]
fn new_exit_codes_are_in_the_post_v1_range() {
  // R78 fixed the codes at 72/73/74; the constants are part of the
  // public CLI contract.
  assert_eq!(INIT_ABORTED, 72);
  assert_eq!(INIT_DOWNLOAD_FAILED, 73);
  assert_eq!(INIT_SMOKE_FAILED, 74);
}

#[test]
fn pull_failed_remains_69_for_standalone_pull() {
  // Distinct from INIT_DOWNLOAD_FAILED so scripts can branch on
  // "wizard's download step" vs "standalone llamastash pull".
  assert_eq!(PULL_FAILED, 69);
  assert_ne!(PULL_FAILED, INIT_DOWNLOAD_FAILED);
  assert_ne!(INIT_ABORTED, UNKNOWN);
}

#[test]
fn init_recommended_flag_parses() {
  match parse(&["init", "--recommended"]).command {
    Some(Command::Init(args)) => {
      assert!(args.recommended);
      assert!(!args.yes);
    }
    other => panic!("expected init, got {other:?}"),
  }
}

#[test]
fn init_yes_is_hidden_alias_still_parses() {
  match parse(&["init", "--yes"]).command {
    Some(Command::Init(args)) => {
      assert!(args.yes);
      assert!(!args.recommended);
    }
    other => panic!("expected init, got {other:?}"),
  }
}

#[test]
fn init_recommended_and_yes_are_combinable_no_mutex() {
  match parse(&["init", "--recommended", "--yes"]).command {
    Some(Command::Init(args)) => {
      assert!(args.recommended);
      assert!(args.yes);
    }
    other => panic!("expected init, got {other:?}"),
  }
}

#[test]
fn init_install_each_simple_variant() {
  for (raw, expected) in [
    ("brew", InstallOverride::Brew),
    ("gh-releases", InstallOverride::GhReleases),
    ("existing", InstallOverride::Existing),
  ] {
    match parse(&["init", "--install", raw]).command {
      Some(Command::Init(args)) => assert_eq!(args.install, Some(expected)),
      other => panic!("expected init, got {other:?}"),
    }
  }
}

#[test]
fn init_install_custom_path_parses_to_pathbuf() {
  match parse(&["init", "--install", "custom:/usr/local/bin/llama-server"]).command {
    Some(Command::Init(args)) => assert_eq!(
      args.install,
      Some(InstallOverride::Custom(PathBuf::from(
        "/usr/local/bin/llama-server"
      )))
    ),
    other => panic!("expected init, got {other:?}"),
  }
}

#[test]
fn init_install_custom_empty_path_errors() {
  let result = Cli::try_parse_from(["llamastash", "init", "--install", "custom:"]);
  assert!(
    result.is_err(),
    "empty custom path must error at parse time"
  );
}

#[test]
fn init_install_unknown_value_errors_with_valid_choices_listed() {
  let result = Cli::try_parse_from(["llamastash", "init", "--install", "frobnicate"]);
  let err = result.expect_err("frobnicate must be rejected");
  let msg = err.to_string();
  for token in ["brew", "gh-releases", "existing", "custom:<PATH>"] {
    assert!(
      msg.contains(token),
      "error must list `{token}` as a valid choice, got: {msg}"
    );
  }
}

#[test]
fn init_model_recommended_and_none() {
  for (raw, expected) in [
    ("recommended", ModelOverride::Recommended),
    ("none", ModelOverride::None),
  ] {
    match parse(&["init", "--model", raw]).command {
      Some(Command::Init(args)) => assert_eq!(args.model, Some(expected)),
      other => panic!("expected init, got {other:?}"),
    }
  }
}

#[test]
fn init_model_owner_repo_parses_to_paste() {
  match parse(&["init", "--model", "bartowski/Llama-3.2-3B-GGUF"]).command {
    Some(Command::Init(args)) => assert_eq!(
      args.model,
      Some(ModelOverride::Paste(
        "bartowski/Llama-3.2-3B-GGUF".to_string()
      ))
    ),
    other => panic!("expected init, got {other:?}"),
  }
}

#[test]
fn init_model_invalid_no_slash_errors() {
  let result = Cli::try_parse_from(["llamastash", "init", "--model", "no-slash-here"]);
  assert!(result.is_err());
}

#[test]
fn init_config_step_write_and_skip() {
  for (raw, expected) in [
    ("write", ConfigOverride::Write),
    ("skip", ConfigOverride::Skip),
  ] {
    match parse(&["init", "--config-step", raw]).command {
      Some(Command::Init(args)) => assert_eq!(args.config_choice, Some(expected)),
      other => panic!("expected init, got {other:?}"),
    }
  }
}

#[test]
fn init_all_new_flags_combined() {
  match parse(&[
    "init",
    "--recommended",
    "--install",
    "brew",
    "--model",
    "bartowski/Llama-3.2-3B-GGUF",
    "--config-step",
    "write",
  ])
  .command
  {
    Some(Command::Init(args)) => {
      assert!(args.recommended);
      assert_eq!(args.install, Some(InstallOverride::Brew));
      assert_eq!(
        args.model,
        Some(ModelOverride::Paste(
          "bartowski/Llama-3.2-3B-GGUF".to_string()
        ))
      );
      assert_eq!(args.config_choice, Some(ConfigOverride::Write));
    }
    other => panic!("expected init, got {other:?}"),
  }
}

#[test]
fn init_only_and_skip_mutex_still_holds_after_new_flags() {
  let result = Cli::try_parse_from([
    "llamastash",
    "init",
    "--recommended",
    "--only",
    "server",
    "--skip",
    "config",
  ]);
  assert!(
    result.is_err(),
    "--only and --skip mutex must survive after new flags land"
  );
}
