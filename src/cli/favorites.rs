//! `llamatui favorites {list|add|remove}`.

use serde_json::{json, Value};

use crate::cli::cli_args::{Cli, FavoritesAction, FavoritesArgs};
use crate::cli::client::connect_or_spawn;
use crate::cli::exit_codes::{CliExit, CliResult};
use crate::cli::output::pretty_json;
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
      if as_json {
        println!("{}", pretty_json(&body));
      } else {
        let arr = body
          .get("favorites")
          .and_then(Value::as_array)
          .cloned()
          .unwrap_or_default();
        if arr.is_empty() {
          println!("(no favorites)");
        } else {
          for fav in &arr {
            let path = fav
              .get("id")
              .and_then(|id| id.get("path"))
              .and_then(Value::as_str)
              .unwrap_or("?");
            println!("{path}");
          }
        }
      }
      Ok(())
    }
    FavoritesAction::Add { model } => {
      let rows = fetch_catalog(&mut client).await?;
      let row = resolve_model(&rows, &model)?;
      let body = client
        .call("favorite_add", Some(json!({"model_path": &row.path})))
        .await
        .map_err(CliExit::from_client_error)?;
      let added = body.get("added").and_then(Value::as_bool).unwrap_or(false);
      if added {
        println!("favorited {}", row.name());
      } else {
        println!("{} already favorited (no-op)", row.name());
      }
      Ok(())
    }
    FavoritesAction::Remove { model } => {
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
      if removed {
        println!("unfavorited {}", row.name());
      } else {
        println!("{} was not in favorites (no-op)", row.name());
      }
      Ok(())
    }
  }
}
