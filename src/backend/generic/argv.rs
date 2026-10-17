//! argv and env for one generic launch.
//!
//! argv = `args` (placeholders filled) + each set knob not referenced by a
//! placeholder, as `<flag> <value>` (a `switch` knob: the bare flag when
//! `true`) in declaration order + launch extras. The
//! entry's own `args` skip the extras denylist: they carry `{port}` and
//! `{host}` by design.

use std::collections::BTreeSet;
use std::ffi::OsString;

use crate::launch::knobs::KnobSet;

use super::config::{placeholders_in, substitute, GenericServer};

/// Children bind loopback even when the proxy is on the LAN.
pub const CHILD_HOST: &str = "127.0.0.1";

/// What a launch spawns with, beyond the binary.
#[derive(Debug, Clone, PartialEq)]
pub struct Composed {
  pub argv: Vec<OsString>,
  pub env: Vec<(String, OsString)>,
}

/// Each declared knob's id, flag, value (the resolved set's, else the entry's
/// `default`, else none) and whether it is a `switch`.
fn knob_values(
  entry: &GenericServer,
  knobs: &KnobSet,
) -> Vec<(String, String, Option<String>, bool)> {
  entry
    .knobs
    .iter()
    .map(|decl| {
      let spec = decl.spec();
      let id = spec.knob_id();
      let value = knobs
        .iter()
        .find(|(k, _)| k.as_str() == id)
        .and_then(|(_, v)| v.set_value())
        .map(|s| s.to_arg())
        .or(spec.default.clone());
      (id, spec.flag.trim().to_string(), value, spec.switch)
    })
    .collect()
}

/// Compose argv + env, or the refusal for a placeholder knob with no value.
pub fn compose(
  entry: &GenericServer,
  knobs: &KnobSet,
  extras: &[OsString],
  port: u16,
  published_name: &str,
  model: &std::path::Path,
) -> Result<Composed, String> {
  let values = knob_values(entry, knobs);
  let referenced: BTreeSet<String> = entry.placeholders().into_iter().collect();
  for (id, _, value, _) in &values {
    if referenced.contains(id) && value.is_none() {
      return Err(format!(
        "backend.generic entry `{}`: knob `{id}` is referenced by a placeholder but has no value; \
         set it or give it a `default`",
        entry.name
      ));
    }
  }
  let port_s = port.to_string();
  let lookup = |key: &str| -> Option<String> {
    match key {
      "port" => Some(port_s.clone()),
      "host" => Some(CHILD_HOST.to_string()),
      "name" => Some(published_name.to_string()),
      "model" => Some(model.to_string_lossy().into_owned()),
      _ => values
        .iter()
        .find(|(id, _, _, _)| id == key)
        .and_then(|(_, _, v, _)| v.clone()),
    }
  };

  let mut argv: Vec<OsString> = entry
    .args
    .iter()
    .map(|a| OsString::from(substitute(a, &lookup)))
    .collect();
  for (id, flag, value, switch) in &values {
    if referenced.contains(id) {
      continue;
    }
    match (value, switch) {
      (Some(v), true) if v.trim() == "true" => argv.push(flag.into()),
      (Some(_), true) | (None, _) => {}
      (Some(v), false) => {
        argv.push(flag.into());
        argv.push(v.into());
      }
    }
  }
  argv.extend(crate::launch::params::strip_forbidden_extras(
    extras,
    &[],
    &[],
    "generic",
  ));

  let env = entry
    .env
    .iter()
    .map(|(k, v)| {
      (
        k.clone(),
        OsString::from(expand_home(&substitute(v, &lookup))),
      )
    })
    .collect();
  Ok(Composed { argv, env })
}

/// A leading `~/` in a substituted env value, expanded like `binary` is. The
/// rest of the string is left alone: only the leading `~/` is unambiguous, and a
/// `LD_LIBRARY_PATH` written as `~/.cache/...` otherwise fails at exec with a
/// loader error that does not point back at the path.
fn expand_home(value: &str) -> String {
  if value.starts_with("~/") {
    return crate::util::paths::expand_user_path(std::path::Path::new(value))
      .to_string_lossy()
      .into_owned();
  }
  value.to_string()
}

/// Knob ids of `entry` referenced by a placeholder in `args` or `env`.
pub fn referenced_knobs(entry: &GenericServer) -> BTreeSet<String> {
  entry
    .args
    .iter()
    .chain(entry.env.values())
    .flat_map(|s| placeholders_in(s))
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::backend::generic::config::GenericConfig;

  fn entries() -> GenericConfig {
    yaml_serde::from_str(
      r#"
servers:
  - name: argv-gufo
    binary: /opt/gufo
    args: [serve, --host, "{host}", --port, "{port}", llm, --served-model-name, "{name}"]
    knobs:
      - {flag: --context, ctx: true, default: "262144"}
      - {flag: --speculative, default: mtp}
      - {flag: -t, id: temperature, default: "1.0"}
      - --seed
  - name: argv-halogen
    binary: /opt/halogen.sh
    args: ["{port}"]
    knobs:
      - {flag: --ctx-window, id: halogen-ctx, ctx: true, default: "65536"}
      - {flag: --temperature, default: "1.0"}
      - --needs-value
    env:
      HALOGEN_CTX: "{halogen-ctx}"
      HALOGEN_TEMPERATURE: "{temperature}"
      NEEDS: "{needs-value}"
"#,
    )
    .unwrap()
  }

  const NO_MODEL: &str = "";
  #[test]
  fn an_env_value_with_a_leading_home_prefix_resolves() {
    // A loader error at exec (`libfoo.so.1: cannot open shared object file`,
    // exit 127) does not point back at the path, so `~/` must resolve here.
    let mut entry = entries().servers[0].clone();
    entry
      .env
      .insert("LD_LIBRARY_PATH".into(), "~/lib/{name}".into());
    entry.env.insert("KEEP".into(), "a:~/b".into());
    let out = compose(
      &entry,
      &KnobSet::new(),
      &[],
      41000,
      "argv-gufo",
      std::path::Path::new(NO_MODEL),
    )
    .unwrap();
    let home = crate::util::paths::home_dir().expect("home dir");
    let get = |k: &str| {
      out
        .env
        .iter()
        .find(|(name, _)| name == k)
        .map(|(_, v)| v.to_string_lossy().into_owned())
    };
    assert_eq!(
      get("LD_LIBRARY_PATH"),
      Some(home.join("lib/argv-gufo").to_string_lossy().into_owned())
    );
    assert_eq!(get("KEEP").as_deref(), Some("a:~/b"), "only a leading `~/`");
  }

  fn strs(v: &[OsString]) -> Vec<String> {
    v.iter().map(|s| s.to_string_lossy().into_owned()).collect()
  }

  #[test]
  fn defaults_fill_the_knobs_then_extras_follow() {
    let c = entries();
    let out = compose(
      &c.servers[0],
      &KnobSet::new(),
      &["--extra".into(), "1".into()],
      41000,
      "argv-gufo",
      std::path::Path::new(NO_MODEL),
    )
    .unwrap();
    assert_eq!(
      strs(&out.argv),
      [
        "serve",
        "--host",
        "127.0.0.1",
        "--port",
        "41000",
        "llm",
        "--served-model-name",
        "argv-gufo",
        "--context",
        "262144",
        "--speculative",
        "mtp",
        "-t",
        "1.0",
        "--extra",
        "1"
      ],
      "unset `--seed` with no default emits nothing"
    );
  }

  #[test]
  fn a_named_launch_publishes_its_address_as_name() {
    let c = entries();
    let out = compose(
      &c.servers[0],
      &KnobSet::new(),
      &[],
      1,
      "argv-gufo@coder",
      std::path::Path::new(NO_MODEL),
    )
    .unwrap();
    assert!(strs(&out.argv).contains(&"argv-gufo@coder".to_string()));
  }

  #[test]
  fn a_switch_sends_its_bare_flag_only_when_on() {
    let c: GenericConfig = yaml_serde::from_str(
      r#"
servers:
  - name: argv-switch
    binary: /opt/s
    args: []
    knobs:
      - {flag: --stream, switch: true}
      - {flag: --warm, switch: true, default: "true"}
      - {flag: --cold, switch: true, default: "false"}
"#,
    )
    .unwrap();
    let run = |knobs: &KnobSet| {
      let out = compose(
        &c.servers[0],
        knobs,
        &[],
        1,
        "n",
        std::path::Path::new(NO_MODEL),
      )
      .unwrap();
      strs(&out.argv)
    };
    assert_eq!(run(&KnobSet::new()), ["--warm"]);

    let mut knobs = KnobSet::new();
    knobs.set_scalar(
      crate::launch::knobs::KnobId("stream"),
      crate::launch::knobs::Scalar::Bool(true),
    );
    knobs.set_scalar(
      crate::launch::knobs::KnobId("warm"),
      crate::launch::knobs::Scalar::Bool(false),
    );
    assert_eq!(run(&knobs), ["--stream"]);
  }

  #[test]
  fn env_referenced_knobs_land_in_env_not_argv() {
    let c = entries();
    let mut knobs = KnobSet::new();
    knobs.set_scalar(
      crate::launch::knobs::KnobId("needs-value"),
      crate::launch::knobs::Scalar::Str("x".into()),
    );
    let out = compose(
      &c.servers[1],
      &knobs,
      &[],
      42000,
      "argv-halogen",
      std::path::Path::new(NO_MODEL),
    )
    .unwrap();
    assert_eq!(strs(&out.argv), ["42000"]);
    let env: std::collections::BTreeMap<_, _> = out
      .env
      .iter()
      .map(|(k, v)| (k.as_str(), v.to_string_lossy().into_owned()))
      .collect();
    assert_eq!(env["HALOGEN_CTX"], "65536");
    assert_eq!(env["HALOGEN_TEMPERATURE"], "1.0");
    assert_eq!(env["NEEDS"], "x");
  }

  #[test]
  fn an_env_knob_with_no_value_refuses_the_launch() {
    let c = entries();
    let err = compose(
      &c.servers[1],
      &KnobSet::new(),
      &[],
      1,
      "argv-halogen",
      std::path::Path::new(NO_MODEL),
    )
    .unwrap_err();
    assert!(
      err.contains("argv-halogen") && err.contains("needs-value"),
      "{err}"
    );
  }

  #[test]
  fn extras_are_denylisted_but_entry_args_are_not() {
    let c = entries();
    let out = compose(
      &c.servers[0],
      &KnobSet::new(),
      &["--port".into(), "9".into()],
      41000,
      "argv-gufo",
      std::path::Path::new(NO_MODEL),
    )
    .unwrap();
    let argv = strs(&out.argv);
    assert_eq!(argv.iter().filter(|a| *a == "--port").count(), 1);
    assert!(!argv.contains(&"9".to_string()));
  }
}
