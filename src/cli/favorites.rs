//! `llamastash favorites {list|add|remove}`.

use serde_json::{json, Value};

use crate::cli::cli_args::{Cli, FavoritesAction, FavoritesArgs};
use crate::cli::client::connect_or_spawn;
use crate::cli::colors;
use crate::cli::exit_codes::{CliExit, CliResult};
use crate::cli::output::{favorites_json, pretty_json};
use crate::cli::resolve::{fetch_catalog, resolve_model};
use crate::config::Config;

pub async fn handle(args: FavoritesArgs, cli: &Cli, config: &Config) -> CliResult {
  let mut client = connect_or_spawn(cli, config).await?;

  match args.action {
    FavoritesAction::List { json: as_json } => {
      let body = client
        .call("favorite_list", None)
        .await
        .map_err(CliExit::from_client_error)?;
      let arr = body
        .get("favorites")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
      if as_json {
        // Project through `favorites_json` instead of dumping the
        // daemon body verbatim — without this every future daemon
        // field added to `favorite_list` would silently become part
        // of the CLI agent contract.
        println!("{}", pretty_json(&favorites_json(&arr)));
      } else {
        print!("{}", render_favorites_human(&arr));
      }
      Ok(())
    }
    FavoritesAction::Add {
      model,
      json: as_json,
    } => {
      let rows = fetch_catalog(&mut client).await?;
      let row = resolve_model(&rows, &model)?;
      let body = client
        .call("favorite_add", Some(json!({"model_path": &row.path})))
        .await
        .map_err(CliExit::from_client_error)?;
      let added = body.get("added").and_then(Value::as_bool).unwrap_or(false);
      if as_json {
        let out = json!({
          "action": "add",
          "model": row.name(),
          "path": row.path,
          "added": added,
          "already_present": !added,
        });
        println!("{}", pretty_json(&out));
      } else if !cli.quiet {
        if added {
          println!("{}", colors::success(&format!("favorited {}", row.name())));
        } else {
          println!(
            "{}",
            colors::dim(&format!("{} already favorited (no-op)", row.name()))
          );
        }
      }
      Ok(())
    }
    FavoritesAction::Remove {
      model,
      json: as_json,
    } => {
      let rows = fetch_catalog(&mut client).await?;
      let row = resolve_model(&rows, &model)?;
      let body = client
        .call("favorite_remove", Some(json!({"model_path": &row.path})))
        .await
        .map_err(CliExit::from_client_error)?;
      let removed = body
        .get("removed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
      if as_json {
        let out = json!({
          "action": "remove",
          "model": row.name(),
          "path": row.path,
          "removed": removed,
          "already_absent": !removed,
        });
        println!("{}", pretty_json(&out));
      } else if !cli.quiet {
        if removed {
          println!(
            "{}",
            colors::success(&format!("unfavorited {}", row.name()))
          );
        } else {
          println!(
            "{}",
            colors::dim(&format!("{} was not in favorites (no-op)", row.name()))
          );
        }
      }
      Ok(())
    }
  }
}

/// Pure renderer for `favorites list` human output. One path per line.
/// On TTY collapses `$HOME → ~` and appends a dim `(N favorites)`
/// footer; piped consumers see verbatim absolute paths with no footer.
fn render_favorites_human(arr: &[Value]) -> String {
  use std::fmt::Write as _;
  if arr.is_empty() {
    return format!("{}\n", colors::dim("(no favorites)"));
  }
  let tty = console::colors_enabled();
  let mut out = String::new();
  for fav in arr {
    let path = crate::cli::output::row_path(fav).unwrap_or("?");
    if tty {
      let _ = writeln!(out, "{}", colors::path(path));
    } else {
      let _ = writeln!(out, "{path}");
    }
  }
  if tty {
    let _ = writeln!(out, "{}", colors::count(arr.len(), "favorites"));
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cli::test_lock::serialize;
  use serde_json::json;
  use std::sync::MutexGuard;

  struct ColorGuard {
    _lock: MutexGuard<'static, ()>,
    prior: bool,
  }

  impl ColorGuard {
    fn set(enabled: bool) -> Self {
      let g = Self {
        _lock: serialize(),
        prior: console::colors_enabled(),
      };
      console::set_colors_enabled(enabled);
      g
    }
  }

  impl Drop for ColorGuard {
    fn drop(&mut self) {
      console::set_colors_enabled(self.prior);
    }
  }

  fn fav(path: &str) -> Value {
    json!({"id": {"path": path}})
  }

  #[test]
  fn render_favorites_human_empty_returns_dim_sentinel() {
    let _g = ColorGuard::set(false);
    let out = render_favorites_human(&[]);
    assert_eq!(out, "(no favorites)\n");
  }

  #[test]
  fn render_favorites_human_tsv_branch_is_byte_stable() {
    // Non-TTY: one absolute path per line, no count footer, no ANSI.
    let _g = ColorGuard::set(false);
    let arr = vec![fav("/m/qwen.gguf"), fav("/m/phi.gguf")];
    let out = render_favorites_human(&arr);
    assert_eq!(out, "/m/qwen.gguf\n/m/phi.gguf\n");
  }

  #[test]
  fn render_favorites_human_tty_branch_appends_count_footer() {
    let _g = ColorGuard::set(true);
    let arr = vec![fav("/m/qwen.gguf"), fav("/m/phi.gguf")];
    let out = render_favorites_human(&arr);
    let plain = console::strip_ansi_codes(&out);
    assert!(plain.contains("/m/qwen.gguf"));
    assert!(plain.contains("/m/phi.gguf"));
    assert!(plain.contains("(2 favorites)"), "footer missing: {plain:?}");
  }
}
