//! The one single-select prompt the CLI and the init wizard share, plus the
//! `start` / `stop` pickers built on it when those commands are called without a
//! positional argument.
//!
//! Every picker refuses non-interactive contexts up front (no TTY or `--json`)
//! so a piped / CI invocation gets an actionable error instead of a hung prompt.
//! The cliclack call itself runs inside `spawn_blocking` because the underlying
//! terminal reads are sync.

use std::io::{self, BufRead, IsTerminal, Read, Write};

use crate::cli::exit_codes::{CliExit, MODEL_NOT_FOUND, USAGE};
use crate::cli::resolve::{CatalogRow, RunningRow};

/// Why a picker could not run. Each command words its own message around this
/// and maps it to its own exit code: `USAGE` for the run commands,
/// `INIT_ABORTED` for the wizard.
#[derive(Debug)]
pub enum PromptError {
  /// The blocking task holding the terminal read did not join.
  Join(String),
  /// The prompt itself failed: a closed terminal, Ctrl-C, Ctrl-D.
  Interact(String),
}

impl PromptError {
  /// Shape this failure into the caller's message. `join` and `prompt` are the
  /// two prefixes each command has always printed.
  pub fn into_exit(self, code: i32, join: &str, prompt: &str) -> CliExit {
    match self {
      PromptError::Join(e) => CliExit::new(code, format!("{join}: {e}")),
      PromptError::Interact(e) => CliExit::new(code, format!("{prompt}: {e}")),
    }
  }
}

/// Whether a picker may be offered at all: a TTY to draw on and no `--json`.
pub fn interactive(json: bool) -> bool {
  !json && std::io::stdin().is_terminal()
}

/// Print `question` and read one line, answering whether the caller said yes.
/// The one stdin reader behind every `[y/N]` prompt, so the destructive verbs
/// parse the same answer. Anything but a bare `y`/`yes` declines.
pub(crate) fn confirm_line<R: Read, W: Write>(
  input: &mut R,
  out: &mut W,
  question: &str,
) -> io::Result<bool> {
  write!(out, "{question}")?;
  out.flush()?;
  let mut buf = String::new();
  // One line, not the whole stream: a terminal stays open after the answer, and
  // reading to EOF there waits for a second Enter nobody is going to press.
  io::BufReader::new(input).read_line(&mut buf)?;
  Ok(matches!(buf.trim().to_lowercase().as_str(), "y" | "yes"))
}

/// Refuse the picker up front when the caller can't drive it. `session` words
/// its own refusal because a missing model there is a model problem, not a
/// usage one.
pub fn ensure_interactive(json: bool, command: &str) -> Result<(), CliExit> {
  if json {
    return Err(CliExit::new(
      USAGE,
      format!("interactive {command} picker is disabled with --json; pass an explicit argument"),
    ));
  }
  if !std::io::stdin().is_terminal() {
    return Err(CliExit::new(
      USAGE,
      format!("interactive {command} picker requires a TTY; pass an explicit argument"),
    ));
  }
  Ok(())
}

/// One single-select prompt: `title`, the highlighted row, and `(label, hint)`
/// items in display order. Returns the index of the chosen item, so the caller
/// keeps its own item type and its own meaning for each row.
///
/// `items` must not be empty; a caller with nothing to offer errors instead.
pub async fn select_one(
  title: String,
  initial: usize,
  items: Vec<(String, String)>,
) -> Result<usize, PromptError> {
  tokio::task::spawn_blocking(move || {
    let mut select = cliclack::select::<usize>(title).initial_value(initial);
    for (i, (label, hint)) in items.into_iter().enumerate() {
      select = select.item(i, label, hint);
    }
    select.interact()
  })
  .await
  .map_err(|e| PromptError::Join(e.to_string()))?
  .map_err(|e| PromptError::Interact(e.to_string()))
}

/// Open a picker over the catalog and return the chosen row. Wired into `start`
/// when the user omits the model reference, and into `session` when it has no
/// MODEL and nothing is running. The caller guards interactivity itself when it
/// has a refusal of its own to print; `command` names it in this one's text.
pub async fn pick_catalog_row(
  rows: &[CatalogRow],
  command: &str,
  title: &str,
) -> Result<CatalogRow, CliExit> {
  if rows.is_empty() {
    return Err(CliExit::new(MODEL_NOT_FOUND, "no models discovered"));
  }
  let owned: Vec<CatalogRow> = rows.to_vec();
  let items = owned.iter().map(|r| (r.name(), catalog_hint(r))).collect();
  let idx = select_one(title.to_string(), 0, items).await.map_err(|e| {
    e.into_exit(
      USAGE,
      &format!("{command} picker join"),
      &format!("{command} picker"),
    )
  })?;
  Ok(
    owned
      .into_iter()
      .nth(idx)
      .expect("select returns a valid idx"),
  )
}

/// Open a picker over running supervisors and return the chosen `launch_id`.
/// Wired into `stop` when the user omits both `<target>` and `--all`.
pub async fn pick_running_target(rows: &[RunningRow], json: bool) -> Result<String, CliExit> {
  ensure_interactive(json, "stop")?;
  if rows.is_empty() {
    return Err(CliExit::new(MODEL_NOT_FOUND, "no managed launches to stop"));
  }
  let owned: Vec<RunningRow> = rows.to_vec();
  let items = owned
    .iter()
    .map(|r| {
      (
        format!("{lid} {name}", lid = r.launch_id, name = r.name()),
        format!(":{port} {state}", port = r.port, state = r.state),
      )
    })
    .collect();
  let idx = select_one("Pick a launch to stop".to_string(), 0, items)
    .await
    .map_err(|e| e.into_exit(USAGE, "stop picker join", "stop picker"))?;
  Ok(
    owned
      .into_iter()
      .nth(idx)
      .expect("select returns a valid idx")
      .launch_id,
  )
}

/// Hint line for a catalog picker row — mirrors the columns the
/// `list` table shows so the user sees the same identifying info in
/// both surfaces.
fn catalog_hint(r: &CatalogRow) -> String {
  let arch = r.arch.as_deref().unwrap_or("?");
  let quant = r.quant.as_deref().unwrap_or("?");
  let ctx = r
    .native_ctx
    .map(|n| n.to_string())
    .unwrap_or_else(|| "?".to_string());
  format!("{arch} {quant} ctx={ctx}")
}

#[cfg(test)]
mod tests {
  use std::io::Cursor;

  use super::*;

  #[test]
  fn a_prompt_failure_keeps_whichever_message_the_command_uses() {
    let join =
      PromptError::Join("boom".into()).into_exit(USAGE, "start picker join", "start picker");
    assert_eq!(join.message.as_deref(), Some("start picker join: boom"));
    assert_eq!(join.code, USAGE);
    let abort = PromptError::Interact("cancelled".into()).into_exit(
      crate::cli::exit_codes::INIT_ABORTED,
      "init: prompt join failed",
      "init: model prompt",
    );
    assert_eq!(
      abort.message.as_deref(),
      Some("init: model prompt: cancelled")
    );
    assert_eq!(abort.code, crate::cli::exit_codes::INIT_ABORTED);
  }

  #[test]
  fn json_rules_out_a_picker_even_on_a_tty() {
    assert!(!interactive(true));
  }

  /// `y` and `yes` in any case confirm; everything else, including an EOF with
  /// nothing on it, declines.
  #[test]
  fn confirm_line_reads_y_yes_and_nothing_else() {
    for answer in ["y\n", "YES\n", " Y "] {
      let mut out = Vec::new();
      assert!(
        confirm_line(&mut Cursor::new(answer.as_bytes()), &mut out, "go? [y/N] ").expect("read"),
        "`{answer}` should confirm"
      );
    }
    for answer in ["", "\n", "n\n", "No\n", "maybe\n"] {
      let mut out = Vec::new();
      assert!(
        !confirm_line(&mut Cursor::new(answer.as_bytes()), &mut out, "go? [y/N] ").expect("read"),
        "`{answer}` should decline"
      );
    }
  }

  /// A terminal hands over one line and stays open, so the reader has to stop at
  /// the newline. This reader hands over "y\n" and then refuses to give anything
  /// else, which is what waiting for EOF would do to a real prompt.
  #[test]
  fn confirm_line_stops_at_the_first_line() {
    struct OneLineOnly(&'static [u8]);
    impl Read for OneLineOnly {
      fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.0.is_empty() {
          return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "the terminal is still open",
          ));
        }
        let n = self.0.len().min(buf.len());
        buf[..n].copy_from_slice(&self.0[..n]);
        self.0 = &self.0[n..];
        Ok(n)
      }
    }
    let mut out = Vec::new();
    assert!(
      confirm_line(&mut OneLineOnly(b"y\n"), &mut out, "go? [y/N] ").expect("reads one line"),
      "a typed y confirms without waiting for the end of input"
    );
  }

  /// The question is what the caller sees, so a shared reader cannot quietly
  /// reword one verb's prompt.
  #[test]
  fn confirm_line_prints_the_question_it_is_given() {
    let mut out = Vec::new();
    confirm_line(
      &mut Cursor::new(b"y\n"),
      &mut out,
      "stop 3 managed launch(es)? [y/N] ",
    )
    .expect("read");
    assert_eq!(
      String::from_utf8(out).expect("utf8"),
      "stop 3 managed launch(es)? [y/N] "
    );
  }
}
