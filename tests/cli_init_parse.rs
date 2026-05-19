//! Integration coverage for the `init` clap surface — repeatable +
//! comma-separated `--only`/`--skip`, mutual exclusion, the
//! `--yes --json --offline` triple, and the new exit codes.

use clap::Parser;
use llamadash::cli::cli_args::{Cli, Command, InitStep};
use llamadash::cli::exit_codes::{
  INIT_ABORTED, INIT_DOWNLOAD_FAILED, INIT_SMOKE_FAILED, PULL_FAILED, UNKNOWN,
};

fn parse(argv: &[&str]) -> Cli {
  Cli::try_parse_from(std::iter::once("llamadash").chain(argv.iter().copied()))
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
  let result = Cli::try_parse_from(["llamadash", "init", "--only", "server", "--skip", "config"]);
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
  // "wizard's download step" vs "standalone llamadash pull".
  assert_eq!(PULL_FAILED, 69);
  assert_ne!(PULL_FAILED, INIT_DOWNLOAD_FAILED);
  assert_ne!(INIT_ABORTED, UNKNOWN);
}
