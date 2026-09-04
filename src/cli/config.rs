//! Inspect or edit the active configuration file.

use std::{env, ffi::OsString, path::Path, process::Command};

use serde::Serialize;

use crate::{
  cli::{cli_args::ConfigAction, exit_codes, Cli, CliExit, CliResult},
  config::Config,
};

/// Handle the `config` command.
pub fn handle(action: Option<ConfigAction>, cli: &Cli, config: &Config) -> CliResult {
  match action {
    None => edit(cli),
    Some(ConfigAction::Bindings) => print_bindings(config),
  }
}

/// Start `$EDITOR` with the active config path and wait for it to exit.
fn edit(cli: &Cli) -> CliResult {
  let path = crate::config::config_path(cli.config.clone())
    .ok_or_else(|| CliExit::new(exit_codes::UNKNOWN, "could not resolve config file path"))?;
  let editor = env::var_os("EDITOR")
    .filter(|value| !value.is_empty())
    .ok_or_else(|| {
      CliExit::new(
        exit_codes::USAGE,
        "the EDITOR environment variable is not set",
      )
    })?;
  let status = editor_command(&editor, &path).status().map_err(|error| {
    CliExit::new(
      exit_codes::UNKNOWN,
      format!("could not start the editor specified by EDITOR: {error}"),
    )
  })?;
  if status.success() {
    Ok(())
  } else {
    Err(CliExit::new(
      exit_codes::UNKNOWN,
      format!("the editor specified by EDITOR exited with {status}"),
    ))
  }
}

/// Print the active keybinding overrides in a block that can be pasted into a
/// config file or copied to another profile.
fn print_bindings(config: &Config) -> CliResult {
  print!("{}", bindings_yaml(config)?);
  Ok(())
}

fn bindings_yaml(config: &Config) -> Result<String, CliExit> {
  #[derive(Serialize)]
  struct Bindings {
    keybindings: std::collections::BTreeMap<&'static str, String>,
  }

  yaml_serde::to_string(&Bindings {
    keybindings: crate::tui::keybindings::KeyMap::effective_config_bindings(&config.keybindings),
  })
  .map_err(|error| {
    CliExit::new(
      exit_codes::UNKNOWN,
      format!("could not serialize keybindings as YAML: {error}"),
    )
  })
}

fn editor_command(editor: &OsString, path: &Path) -> Command {
  let mut command = Command::new(editor);
  command.arg(path);
  command
}

#[cfg(test)]
mod tests {
  use std::{collections::BTreeMap, ffi::OsString, path::Path};

  use super::{bindings_yaml, editor_command};
  use crate::config::Config;

  #[test]
  fn editor_receives_the_config_path_as_its_only_argument() {
    let path = Path::new("/tmp/llamastash-config.yaml");
    let command = editor_command(&OsString::from("editor"), path);

    assert_eq!(command.get_program(), "editor");
    assert_eq!(command.get_args().collect::<Vec<_>>(), [path.as_os_str()]);
  }

  #[test]
  fn bindings_export_serializes_a_pasteable_keybindings_block() {
    let config = Config {
      keybindings: BTreeMap::from([
        ("quit".to_string(), "ctrl+q".to_string()),
        ("toggle_help".to_string(), "f1".to_string()),
      ]),
      ..Config::default()
    };

    assert_eq!(
      bindings_yaml(&config).unwrap(),
      "keybindings:\n  cancel: esc\n  cancel_download: ctrl+x\n  clear_filter: esc\n  cycle_pane_ratio: alt+l\n  cycle_theme: t\n  cycle_theme_prev: T\n  cycle_value_next: right\n  cycle_value_prev: left\n  delete_model: ctrl+d\n  enter_edit: e\n  exit_edit: esc\n  focus_chat_tab: C\n  focus_list: M\n  focus_logs_tab: L\n  focus_settings_tab: S\n  go_bottom: G\n  go_top: g\n  insert_newline: shift+enter\n  kill_daemon: ctrl+k\n  launch_named: alt+enter\n  move_down: down\n  move_up: up\n  next_field: down\n  next_focus: tab\n  open_filter: /\n  open_hf_dialog: P\n  open_launch_picker: enter\n  page_down: pgdn\n  page_up: pgup\n  prev_field: up\n  prev_focus: shift+tab\n  quit: ctrl+q\n  restart_daemon: ctrl+r\n  save_preset: ctrl+p\n  send_chat: enter\n  stop_model: ctrl+s\n  submit: enter\n  toggle_auto_scroll: s\n  toggle_device: space\n  toggle_favorite: f\n  toggle_help: f1\n  toggle_think_collapse: r\n  yank_curl: c\n  yank_path: p\n  yank_url: u\n"
    );
  }
}
