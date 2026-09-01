//! Neutral launch IR: the backend-agnostic types every backend reads.
//!
//! [`LaunchParams`] carries the user's launch choices, [`crate::launch::knobs::KnobSet`] the
//! tuning surface, and the layered resolver
//! ([`resolve_layered`](crate::launch::knobs::resolve_layered),
//! [`seed_layerless`](crate::launch::knobs::seed_layerless)) merges the
//! precedence chain into a [`Resolved`](crate::launch::knobs::Resolved) set.
//! The per-backend argv emitter lives with its backend — llama.cpp's is
//! `crate::backend::llama_cpp::compose`.
//!
//! `forbidden_in_extras` / `is_forbidden_head` enforce the loopback-only
//! and same-UID contract: a curated denylist (`--host`, `--listen`,
//! `--bind`, `--api-key`, `--ssl-*`) is refused. They live here, not in a
//! backend, because both the llama.cpp extras strip and the native-knob
//! translation reuse the same guard. llama-server honours the
//! last-occurrence of a flag, so without this guard a trailing
//! `--host 0.0.0.0` in `extras` would expose the model to the LAN.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::launch::mode::LaunchMode;

/// Flags refused in `LaunchParams.extras` because they would break
/// the loopback-only / same-UID security contract documented in
/// `docs/architecture.md`, or the daemon's ownership of the launch.
/// Match is case-insensitive on the flag itself; `--ssl-*` matches any
/// flag starting with that prefix.
///
/// `--port` is here for the ownership half, not the security one. Every
/// engine takes the last `--port` on its argv, so an extras copy silently
/// won over the port the daemon reserved: the server bound the user's number
/// while readiness probed the reserved one, leaving the launch stuck in
/// `loading` with a fully-loaded model on a port nothing tracked. Use the
/// first-class `--port` flag, which reserves what it asks for.
pub const FORBIDDEN_ADVANCED_PREFIXES: &[&str] = &[
  "--host",
  "--listen",
  "--bind",
  "--api-key",
  "--ssl-",
  "--port",
];

/// Whether `head` hits the loopback/credential denylist. Shared with the
/// native-knob translation entry point ([`crate::launch::native_knobs`]) so a
/// backend's free-text knob value can't smuggle `--host`/`--api-key` past the
/// same guard `compose` applies to extras.
pub(crate) fn is_forbidden_head(head: &str) -> bool {
  head_hits_prefixes(head, FORBIDDEN_ADVANCED_PREFIXES)
}

/// [`is_forbidden_head`] extended with a backend's own network-affecting
/// heads (ds4 adds `--cors` / `--dist-`). A prefix ending in `-` matches by
/// `starts_with`; everything else matches exactly — same rule as the base set.
pub(crate) fn is_forbidden_head_ext(head: &str, extra: &[&str]) -> bool {
  is_forbidden_head(head) || head_hits_prefixes(head, extra)
}

fn head_hits_prefixes(head: &str, prefixes: &[&str]) -> bool {
  let lower = head.to_ascii_lowercase();
  prefixes
    .iter()
    .any(|p| lower == *p || (p.ends_with('-') && lower.starts_with(&p.to_ascii_lowercase())))
}

/// Flag heads whose adjacent value is a secret and must be hidden
/// before display in a log line, error message, or terminal echo.
/// Shared between [`forbidden_in_extras`] and [`redact_for_display`]
/// so both surfaces redact the same set.
const SECRET_BEARING_PREFIXES: &[&str] = &["--api-key", "--ssl-"];

fn is_secret_head(head: &str) -> bool {
  let lower = head.to_ascii_lowercase();
  SECRET_BEARING_PREFIXES
    .iter()
    .any(|p| lower == *p || (p.ends_with('-') && lower.starts_with(p)))
}

/// Returns the subset of `extras` flags that hit the denylist, with
/// secret-bearing values redacted (`--api-key=foo` → `--api-key=<value-redacted>`).
/// Callers must never display the *raw* extras list — only the
/// redacted strings returned here — so a typo'd secret can't land in
/// scrollback or daemon error logs.
///
/// Only the equals-form (`--api-key=foo`) needs explicit redaction
/// here: space-form values (`["--api-key", "foo"]`) arrive as their
/// own free-standing tokens, and `"foo"` on its own doesn't match
/// any forbidden head — so it's silently passed through this filter.
/// The launch is still refused on the basis of the `--api-key` head
/// alone, and the value never lands in the returned banned list.
/// `redact_for_display` does the peek-and-redact for space-form
/// because compose echoes the *full* extras tail back to the user.
pub fn forbidden_in_extras(extras: &[OsString]) -> Vec<String> {
  forbidden_in_extras_ext(extras, &[])
}

/// [`forbidden_in_extras`] extended with a backend's own network-affecting
/// heads (ds4 adds `--cors` / `--dist-`), so a ds4 launch that spells one of
/// those in `--` extras is refused with a clear error rather than silently
/// stripped at spawn.
pub fn forbidden_in_extras_ext(extras: &[OsString], extra_forbidden: &[&str]) -> Vec<String> {
  extras
    .iter()
    .filter_map(|s| {
      let lossy = s.to_string_lossy();
      let head = lossy.split('=').next().unwrap_or(&lossy);
      if !is_forbidden_head_ext(head, extra_forbidden) {
        return None;
      }
      if is_secret_head(head) && lossy.contains('=') {
        Some(format!("{head}=<value-redacted>"))
      } else {
        Some(lossy.into_owned())
      }
    })
    .collect()
}

/// Format an extras list for human display, redacting values that
/// follow secret-bearing prefixes (`--api-key`, `--ssl-*`). Used by
/// the TUI's forbidden-flag inline warning and any other surface
/// that might echo extras back to a log or terminal.
pub fn redact_for_display(extras: &[OsString]) -> String {
  let is_secret = is_secret_head;
  let mut out = String::new();
  let mut iter = extras.iter().peekable();
  while let Some(token) = iter.next() {
    if !out.is_empty() {
      out.push(' ');
    }
    let lossy = token.to_string_lossy();
    if let Some((head, _value)) = lossy.split_once('=') {
      if is_secret(head) {
        out.push_str(head);
        out.push_str("=<value-redacted>");
        continue;
      }
    }
    out.push_str(&lossy);
    if !lossy.contains('=') && is_secret(&lossy) {
      if let Some(next) = iter.peek() {
        let next_lossy = next.to_string_lossy();
        if !next_lossy.starts_with('-') {
          out.push(' ');
          out.push_str("<value-redacted>");
          iter.next();
        }
      }
    }
  }
  out
}

/// Which inference backend should run a launch.
///
/// This is a *launch-level* choice, not a translated knob — "which backend"
/// has no `llama-server` argv form, so it rides on [`LaunchParams`] rather
/// than [`crate::launch::knobs::KnobSet`]. The default [`BackendChoice::Auto`] runs the R13
/// identity rule (GGUF → llama.cpp, registry → its owning backend); an
/// explicit variant overrides it. Resolved by
/// [`crate::backend::resolve_backend`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum BackendChoice {
  /// Pick automatically from the model's identity + the header routing signal.
  #[default]
  Auto,
  /// Force a specific backend by its **id** (`--backend <id>`, or a persisted
  /// resolved-backend tag). Backend-agnostic: the id is validated against the
  /// registry at the CLI / IPC boundary, and an unknown id falls back to the
  /// identity rule in [`crate::backend::resolve_backend`]. Adding a backend
  /// needs no edit here — the id is just data.
  Explicit(String),
}

impl BackendChoice {
  /// Stable lowercase label for CLI parsing / JSON projection — `"auto"` or the
  /// backend id. The wire form (the custom [`serde::Serialize`] below) is
  /// exactly this string, so a persisted `"ds4"` / `"llamacpp"` round-trips
  /// byte-for-byte with the old enum encoding.
  /// The pinned backend id, or `None` when this is `Auto`. Callers that need
  /// "which backend's knobs apply" resolve `None` to the default themselves.
  pub fn explicit_id(&self) -> Option<&str> {
    match self {
      BackendChoice::Auto => None,
      BackendChoice::Explicit(id) => Some(id),
    }
  }

  pub fn label(&self) -> &str {
    match self {
      BackendChoice::Auto => "auto",
      BackendChoice::Explicit(id) => id,
    }
  }

  /// Parse a backend id (or `"auto"`) into a choice — the inverse of
  /// [`Self::label`]. `"auto"` → [`Self::Auto`]; any other id →
  /// [`Self::Explicit`]. Names no backend.
  pub fn from_id(id: &str) -> BackendChoice {
    if id == "auto" {
      BackendChoice::Auto
    } else {
      BackendChoice::Explicit(id.to_string())
    }
  }
}

// Persisted / wired as the bare id string (`"auto"`, `"ds4"`, `"llamacpp"`, …),
// identical to the old externally-tagged unit-variant encoding, so `state.json`
// and preset rows stay byte-stable across this refactor.
impl serde::Serialize for BackendChoice {
  fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(self.label())
  }
}

impl<'de> serde::Deserialize<'de> for BackendChoice {
  fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
    let s = String::deserialize(d)?;
    Ok(Self::from_id(&s))
  }
}

/// The user's intent for MTP (multi-token prediction) speculative decoding on a
/// launch. A launch-level, launch-only choice (no `config.yaml` entry
/// — KD2); persisted in `last_params` / presets like any launch choice. The
/// resolved *directive* (what argv to emit) is [`MtpDirective`], computed
/// server-side from this intent plus the model's real capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MtpEnable {
  /// Enable when the model is MTP-capable (embedded nextn head **or** a
  /// separate `mtp-*.gguf` drafter sibling); emit nothing otherwise. Default.
  #[default]
  Auto,
  /// Force on. If the model is not MTP-capable, warn and skip (never
  /// emit-and-brick — turning speculation on for a non-MTP model is a hard
  /// launch failure on the serving backend).
  On,
  /// Never enable MTP for this launch.
  Off,
}

impl MtpEnable {
  /// Serde skip-predicate: the default state writes nothing, so a preset that
  /// never set MTP keeps its previous bytes in `config.yaml`.
  pub fn is_auto(&self) -> bool {
    matches!(self, MtpEnable::Auto)
  }
}

impl MtpEnable {
  /// Stable lowercase label (`"auto"` / `"on"` / `"off"`) for CLI / status.
  pub fn label(self) -> &'static str {
    match self {
      MtpEnable::Auto => "auto",
      MtpEnable::On => "on",
      MtpEnable::Off => "off",
    }
  }

  /// Parse a CLI token into an intent; `None` for an unrecognised value so the
  /// caller can surface a usage error.
  pub fn from_token(s: &str) -> Option<MtpEnable> {
    match s.to_ascii_lowercase().as_str() {
      "auto" => Some(MtpEnable::Auto),
      "on" | "true" | "1" => Some(MtpEnable::On),
      "off" | "false" | "0" => Some(MtpEnable::Off),
      _ => None,
    }
  }

  /// Next stop on the TUI picker's cycle ring (`auto → on → off → auto`
  /// forward, reversed backward). Backend-agnostic — the picker shows one MTP
  /// row for any MTP-capable model, and each backend honors the resolved intent.
  pub fn cycled(self, forward: bool) -> MtpEnable {
    use MtpEnable::*;
    if forward {
      match self {
        Auto => On,
        On => Off,
        Off => Auto,
      }
    } else {
      match self {
        Auto => Off,
        On => Auto,
        Off => On,
      }
    }
  }
}

/// The resolved MTP directive for one launch — what `compose` emits, and the
/// running-row truth `status` reports. Computed server-side in
/// `compose_and_spawn` from [`MtpEnable`] + capability (see
/// [`resolve_mtp_directive`]). Re-resolved each launch (a persisted value is
/// overwritten), so it is additive on the wire: `Some` on a running MTP launch,
/// omitted otherwise (byte-stable for non-MTP launches).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MtpDirective {
  /// Separate draft-head path the serving backend loads as the drafter; `None`
  /// for an embedded head (the base file self-speculates, no separate drafter).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub draft_model: Option<PathBuf>,
}

/// Resolve the effective MTP directive (KD1 hard gate): fold the user's
/// [`MtpEnable`] intent with the model's real capability. `Some` ⇒ this launch
/// speculates; `None` ⇒ it doesn't.
///
/// - `Off` ⇒ `None`. `Auto` ⇒ enable iff capable. `On` ⇒ enable if capable,
///   else push a warning and return `None` (warn + skip, never emit-and-brick).
/// - An embedded head needs no separate drafter; a separate head names one.
///
/// The KD3 deferral — a user already hand-driving speculation through `extras`
/// — is decided by the serving backend ([`crate::backend::Backend::
/// speculation_set_in_extras`]) before this is called, since only it knows its
/// own flag spelling.
pub fn resolve_mtp_directive(
  intent: MtpEnable,
  embedded_capable: bool,
  separate_head: Option<PathBuf>,
  warnings: &mut Vec<String>,
) -> Option<MtpDirective> {
  let directive = || MtpDirective {
    // Embedded head wins (no separate drafter); else the on-disk head.
    draft_model: if embedded_capable {
      None
    } else {
      separate_head.clone()
    },
  };
  let capable = embedded_capable || separate_head.is_some();
  match intent {
    MtpEnable::Off => None,
    MtpEnable::Auto => capable.then(directive),
    MtpEnable::On if capable => Some(directive()),
    MtpEnable::On => {
      warnings.push(
        "MTP forced on (`--mtp on`) but this model is not MTP-capable \
         (no embedded nextn head, no `mtp-*.gguf` drafter sibling) — skipping."
          .to_string(),
      );
      None
    }
  }
}

/// Whether `extras` already carries the flag `head` (space or `=` form).
///
/// The flag spelling is the caller's — a backend passes its own — so this stays
/// a parsing helper rather than knowledge of any one backend's argv.
pub fn extras_have_flag(extras: &[OsString], head: &str) -> bool {
  extras.iter().any(|e| {
    let lossy = e.to_string_lossy();
    let first = lossy.split('=').next().unwrap_or(&lossy);
    first.eq_ignore_ascii_case(head)
  })
}

/// All launch knobs the supervisor reads. Persisted under
/// `last_params: HashMap<ModelIdentity, LaunchParams>` in `state.json`.
///
/// Pre-1.0 schema flip: the old `advanced: Vec<OsString>` field has
/// been replaced with `knobs: crate::launch::knobs::KnobSet` + `extras: Vec<OsString>`.
/// Existing state files from before the flip parse-fail and
/// quarantine to `state.json.broken-<ts>` per `daemon::mod`'s
/// existing path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaunchParams {
  /// Absolute path to the GGUF the user picked (or shard 1 for split
  /// sets).
  pub model_path: PathBuf,
  /// Chosen launch mode (chat / embedding / rerank).
  pub mode: LaunchMode,
  /// Context length. `None` lets `llama-server` use the GGUF's
  /// native value (no `-c` flag).
  ///
  /// **Persistence note:** on a running launch this holds the
  /// *resolved* ctx the supervisor argv-ified (after the
  /// `user > last_used > arch_defaults > builtin > model_default`
  /// chain). It may differ from `knobs.u32(crate::launch::knobs::kid("ctx-size"))`, which holds the
  /// *user-supplied delta* — the field the editor seeds `user_knobs`
  /// from on return. Read `knobs.u32(crate::launch::knobs::kid("ctx-size"))` for source-chip semantics;
  /// read this for what actually shipped on the wire.
  pub ctx: Option<u32>,
  /// Listening port. `None` leaves port allocation to the supervisor.
  pub port: Option<u16>,
  /// Reasoning bundle on/off. When `true`, supervisor appends
  /// `--jinja --reasoning-format deepseek` to the argv.
  ///
  /// **Persistence note:** like `ctx` above, this is the *resolved*
  /// value collapsed to a bool (`None`/`Some(false)` → `false`).
  /// May differ from `knobs.bool(crate::launch::knobs::kid("reasoning"))`, which keeps the tri-state
  /// `Option<bool>` the user actually supplied.
  pub reasoning: bool,
  /// Every knob this launch carries, keyed by the declaring backend's own id
  /// (`crate::launch::knobs`). Emitted before `extras` in the backend's
  /// declaration order; unset ids emit nothing.
  ///
  /// Replaces the old split between a llama.cpp-keyed `crate::launch::knobs::KnobSet` struct and
  /// a parallel stringly-typed `backend_knobs` map. One map means one layering
  /// engine, one persistence shape, and one generated surface per client.
  #[serde(default)]
  pub knobs: crate::launch::knobs::KnobSet,
  /// Free-form argv tail for `llama-server` flags the typed editor
  /// doesn't model (e.g. `--rope-freq-base`, sampling params).
  /// Emitted *after* `knobs` so the last-occurrence wins per
  /// llama-server semantics — same "extras trump bundled" contract
  /// documented on the Settings tab.
  #[serde(default)]
  pub extras: Vec<OsString>,
  /// Optional path to a multimodal projector (mmproj) file. When set,
  /// the supervisor appends `--mmproj <path>` to the llama-server
  /// argv. The file is auto-detected by scanning the parent directory
  /// of the model for a `mmproj-<stem>.gguf` or `mmproj_<stem>.gguf`
  /// companion.
  #[serde(default)]
  pub mmproj_path: Option<PathBuf>,
  /// Which backend runs this launch. Defaults to
  /// [`BackendChoice::Auto`] (the R13 identity rule); an explicit value
  /// overrides per-model. Persisted in last-params like the other choices,
  /// so a returning user keeps their override. `#[serde(default)]` keeps
  /// pre-Phase-2b `state.json` rows loading as `Auto`.
  #[serde(default)]
  pub backend: BackendChoice,
  /// Chosen **server** id — a build/binary of a backend (`llamacpp·vulkan`,
  /// `ds4·ds4`). Determines which binary the launch spawns; persisted in
  /// last-params so a relaunch reuses the build. `None` = no pick (default
  /// binary). `#[serde(default)]` keeps pre-server-abstraction rows loading.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub server: Option<String>,
  /// Config values a backend projects onto its own launch, keyed by the
  /// backend's own name for them.
  ///
  /// Deliberately **not** knobs: these come from `config.yaml`, not from the
  /// user's per-launch intent, so they get no CLI flag, no editor row and no
  /// preset key. Re-derived every launch rather than inherited, so a value
  /// persisted in `last_params` cannot outlive the config flip that changed
  /// it. Empty for a backend that projects nothing.
  #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
  pub launch_config: BTreeMap<String, String>,
  /// MTP (multi-token prediction) speculative-decoding intent for this launch.
  /// Default [`MtpEnable::Auto`] (enable when the model is MTP-capable);
  /// launch-only (no config-file entry — KD2), persisted here in `last_params`
  /// like any launch choice. `#[serde(default)]` keeps older rows loading.
  #[serde(default)]
  pub mtp: MtpEnable,
  /// How many tokens to draft per speculation step, when MTP resolves on.
  /// `None` ⇒ unset, leaving the serving backend on its own default. Launch-only,
  /// persisted like `mtp`; each backend maps it onto its own flag.
  #[serde(default)]
  pub mtp_draft_n: Option<u32>,
  /// Resolved MTP directive for *this* launch (what the serving backend turns
  /// into argv, and the running-row truth `status` reports): `Some` ⇒ this launch
  /// speculates (naming a separate draft head when one is used). Computed
  /// server-side in `compose_and_spawn` from `mtp` + capability
  /// ([`resolve_mtp_directive`]) and re-resolved each launch. Additive on the
  /// wire: omitted when `None`, so non-MTP launches stay byte-stable.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub mtp_directive: Option<MtpDirective>,
}

impl LaunchParams {
  pub fn new(model_path: PathBuf, mode: LaunchMode) -> Self {
    Self {
      model_path,
      mode,
      ctx: None,
      port: None,
      reasoning: false,
      knobs: crate::launch::knobs::KnobSet::new(),
      extras: Vec::new(),
      mmproj_path: None,
      backend: BackendChoice::default(),
      server: None,
      launch_config: BTreeMap::new(),
      mtp: MtpEnable::default(),
      mtp_draft_n: None,
      mtp_directive: None,
    }
  }
}

/// One layer in the precedence chain. The label is reported
/// back in `Resolved.sources` so the editor can render per-row
/// origin chips (`(user)`, `(last used)`, `(arch default)`,
/// `(model default)`, `(server default)`).
///
/// `ArchDefault` covers both the user's yaml `arch_defaults` block
/// and the compiled-in arch table — yaml wins per field at resolve
/// time, but the chip is the same since both are conceptually
/// "what this arch defaults to."
///
/// `ModelDefault` means the value comes from the model file itself
/// (GGUF header for `ctx`, chat template for `reasoning`).
/// `ServerDefault` means no flag is sent and llama-server falls back
/// to its own hardcoded default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayerLabel {
  User,
  /// The model's configured `default:` preset, resolved server-side in
  /// `compose_and_spawn`. Ranks below an explicit `User` choice but above
  /// `LastUsed`, so a standing default overrides the last manual launch
  /// while still letting last_params fill fields the default leaves unset.
  PresetDefault,
  LastUsed,
  ArchDefault,
  ModelDefault,
  ServerDefault,
}

impl LayerLabel {
  /// Human-readable, single-token label rendered in the editor.
  pub fn label(self) -> &'static str {
    match self {
      LayerLabel::User => "user",
      LayerLabel::PresetDefault => "default preset",
      LayerLabel::LastUsed => "last used",
      LayerLabel::ArchDefault => "arch default",
      LayerLabel::ModelDefault => "model default",
      LayerLabel::ServerDefault => "server default",
    }
  }
}

/// Strict-`"1"` env-var read for `LLAMASTASH_BENCH_DISABLE_DEFAULTS`.
/// Any other value (including `"0"`, `"true"`, `"yes"`, empty
/// string, or unset) is treated as "not set." This matches the
/// existing `LLAMASTASH_ASSUME_NON_TTY` pattern in
/// `src/init/prompts.rs` so users have a consistent contract across
/// the bench-internal env vars.
pub(crate) fn bench_disable_defaults_from_env() -> bool {
  std::env::var_os("LLAMASTASH_BENCH_DISABLE_DEFAULTS").is_some_and(|v| v == "1")
}

#[cfg(test)]
mod tests {
  use super::*;

  fn base_params() -> LaunchParams {
    LaunchParams::new(PathBuf::from("/m/model.gguf"), LaunchMode::Chat)
  }

  #[test]
  fn launch_params_defaults_backend_to_auto() {
    assert_eq!(base_params().backend, BackendChoice::Auto);
  }

  #[test]
  fn launch_params_without_backend_field_loads_as_auto() {
    // A pre-Phase-2b last_params row has no `backend` key; #[serde(default)]
    // must load it as Auto so existing state.json keeps working.
    let mut v = serde_json::to_value(base_params()).unwrap();
    v.as_object_mut().unwrap().remove("backend");
    assert!(v.get("backend").is_none());
    let p: LaunchParams = serde_json::from_value(v).unwrap();
    assert_eq!(p.backend, BackendChoice::Auto);
  }

  #[test]
  fn backend_choice_serde_round_trips_as_id_strings() {
    for c in [
      BackendChoice::Auto,
      BackendChoice::Explicit("llamacpp".into()),
      BackendChoice::Explicit("lemonade".into()),
      BackendChoice::Explicit("ds4".into()),
    ] {
      let s = serde_json::to_string(&c).unwrap();
      let back: BackendChoice = serde_json::from_str(&s).unwrap();
      assert_eq!(c, back);
    }
    // Wire value is the bare id string — byte-stable with the old unit-variant
    // encoding, so existing `state.json` / preset rows keep parsing.
    assert_eq!(
      serde_json::to_string(&BackendChoice::Auto).unwrap(),
      "\"auto\""
    );
    assert_eq!(
      serde_json::to_string(&BackendChoice::Explicit("llamacpp".into())).unwrap(),
      "\"llamacpp\""
    );
    assert_eq!(
      serde_json::from_str::<BackendChoice>("\"ds4\"").unwrap(),
      BackendChoice::Explicit("ds4".into())
    );
  }

  #[test]
  fn resolve_mtp_directive_auto_gates_on_capability() {
    let mut w = Vec::new();
    // Auto + embedded → on, no drafter.
    let embedded = resolve_mtp_directive(MtpEnable::Auto, true, None, &mut w);
    assert_eq!(embedded, Some(MtpDirective { draft_model: None }));
    // Auto + separate head only → on, with drafter.
    let head = Some(PathBuf::from("/m/mtp-x.gguf"));
    let separate = resolve_mtp_directive(MtpEnable::Auto, false, head.clone(), &mut w);
    assert_eq!(separate, Some(MtpDirective { draft_model: head }));
    // Auto + not capable → off (no flag, no warning).
    let incapable = resolve_mtp_directive(MtpEnable::Auto, false, None, &mut w);
    assert_eq!(incapable, None);
    assert!(w.is_empty());
  }

  #[test]
  fn resolve_mtp_directive_force_on_non_capable_warns_and_skips() {
    let mut w = Vec::new();
    let d = resolve_mtp_directive(MtpEnable::On, false, None, &mut w);
    assert_eq!(d, None, "force-on a non-capable model must skip, not brick");
    assert_eq!(w.len(), 1, "and warn");
    assert!(w[0].contains("not MTP-capable"));
  }

  #[test]
  fn resolve_mtp_directive_off_never_speculates() {
    let mut w = Vec::new();
    assert_eq!(
      resolve_mtp_directive(MtpEnable::Off, true, None, &mut w),
      None
    );
    assert!(w.is_empty());
  }

  #[test]
  fn extras_have_flag_matches_both_forms_and_nothing_else() {
    // A backend passes its own flag; this only decides presence — space form,
    // `=` form, case-insensitive, and no false hit on a value or a longer name.
    let space = vec![OsString::from("--some-flag"), OsString::from("value")];
    assert!(extras_have_flag(&space, "--some-flag"));
    let eq = vec![OsString::from("--Some-Flag=value")];
    assert!(extras_have_flag(&eq, "--some-flag"));
    let unrelated = vec![OsString::from("--other"), OsString::from("--some-flag-ish")];
    assert!(!extras_have_flag(&unrelated, "--some-flag"));
  }

  #[test]
  fn mtp_enable_from_token_parses_aliases() {
    assert_eq!(MtpEnable::from_token("auto"), Some(MtpEnable::Auto));
    assert_eq!(MtpEnable::from_token("ON"), Some(MtpEnable::On));
    assert_eq!(MtpEnable::from_token("off"), Some(MtpEnable::Off));
    assert_eq!(MtpEnable::from_token("true"), Some(MtpEnable::On));
    assert_eq!(MtpEnable::from_token("0"), Some(MtpEnable::Off));
    assert_eq!(MtpEnable::from_token("nonsense"), None);
  }

  #[test]
  fn mtp_serde_round_trips_and_defaults_auto() {
    // Wire form is the lowercase label; a row without `mtp` loads as Auto.
    let mut p = base_params();
    p.mtp = MtpEnable::On;
    p.mtp_draft_n = Some(4);
    let v = serde_json::to_value(&p).unwrap();
    assert_eq!(v["mtp"], "on");
    assert_eq!(v["mtp_draft_n"], 4);
    let back: LaunchParams = serde_json::from_value(v).unwrap();
    assert_eq!(back.mtp, MtpEnable::On);
    assert_eq!(back.mtp_draft_n, Some(4));
    // Missing `mtp` → Auto (older rows).
    let mut v2 = serde_json::to_value(base_params()).unwrap();
    v2.as_object_mut().unwrap().remove("mtp");
    let d: LaunchParams = serde_json::from_value(v2).unwrap();
    assert_eq!(d.mtp, MtpEnable::Auto);
  }

  #[test]
  fn forbidden_in_extras_flags_loopback_bypass_attempts() {
    let extras = vec![
      OsString::from("--host"),
      OsString::from("0.0.0.0"),
      OsString::from("--LISTEN=0.0.0.0:8080"),
      OsString::from("--threads"),
      OsString::from("8"),
      OsString::from("--api-key"),
      OsString::from("secret"),
      OsString::from("--ssl-key-file"),
      OsString::from("/etc/key.pem"),
    ];
    let banned = forbidden_in_extras(&extras);
    assert!(banned.iter().any(|s| s == "--host"));
    assert!(banned.iter().any(|s| s == "--LISTEN=0.0.0.0:8080"));
    assert!(banned.iter().any(|s| s == "--api-key"));
    assert!(banned.iter().any(|s| s == "--ssl-key-file"));
    assert!(!banned.iter().any(|s| s == "--threads"));
  }

  /// An engine takes the last `--port` on its argv, so an extras copy beat the
  /// port the daemon reserved: the server bound the user's number while
  /// readiness probed the reserved one, and the launch hung in `loading` with
  /// a loaded model on an untracked port. Reproduced on two engines.
  #[test]
  fn forbidden_in_extras_refuses_a_port_that_would_beat_the_reservation() {
    let space = vec![OsString::from("--port"), OsString::from("9999")];
    assert!(forbidden_in_extras(&space).iter().any(|s| s == "--port"));

    let equals = vec![OsString::from("--PORT=9999")];
    assert!(forbidden_in_extras(&equals)
      .iter()
      .any(|s| s == "--PORT=9999"));

    // A longer flag that merely starts with the same letters is untouched:
    // only `--ssl-`-style entries prefix-match.
    let other = vec![OsString::from("--port-scan"), OsString::from("5")];
    assert!(forbidden_in_extras(&other).is_empty());
  }

  #[test]
  fn forbidden_in_extras_redacts_secret_values_in_equals_form() {
    let extras = vec![
      OsString::from("--api-key=supersecret"),
      OsString::from("--ssl-key-file=/etc/key.pem"),
      OsString::from("--host=0.0.0.0"),
    ];
    let banned = forbidden_in_extras(&extras);
    let joined = banned.join(" ");
    assert!(
      !joined.contains("supersecret"),
      "api-key value leaked into banned list: {joined}"
    );
    assert!(
      !joined.contains("/etc/key.pem"),
      "ssl path leaked into banned list: {joined}"
    );
    assert!(banned.iter().any(|s| s == "--api-key=<value-redacted>"));
    assert!(banned
      .iter()
      .any(|s| s == "--ssl-key-file=<value-redacted>"));
    // Non-secret forbidden flags (e.g. --host) keep their value — useful
    // diagnostic and not sensitive.
    assert!(banned.iter().any(|s| s == "--host=0.0.0.0"));
  }

  #[test]
  fn redact_for_display_hides_secret_values_space_form() {
    let extras = vec![
      OsString::from("--api-key"),
      OsString::from("supersecret"),
      OsString::from("--threads"),
      OsString::from("8"),
    ];
    let s = redact_for_display(&extras);
    assert!(!s.contains("supersecret"), "secret leaked: {s}");
    assert!(s.contains("--api-key <value-redacted>"));
    assert!(s.contains("--threads 8"));
  }

  #[test]
  fn redact_for_display_hides_secret_values_equals_form() {
    let extras = vec![OsString::from("--api-key=topsecret")];
    let s = redact_for_display(&extras);
    assert!(!s.contains("topsecret"));
    assert!(s.contains("--api-key=<value-redacted>"));
  }

  #[test]
  fn redact_for_display_handles_ssl_prefix() {
    let extras = vec![
      OsString::from("--ssl-key-file"),
      OsString::from("/etc/k.pem"),
    ];
    let s = redact_for_display(&extras);
    assert!(!s.contains("/etc/k.pem"));
    assert!(s.contains("--ssl-key-file <value-redacted>"));
  }

  #[test]
  fn launch_params_serde_round_trip() {
    let mut p = base_params();
    p.knobs.set_scalar(
      crate::launch::knobs::kid("n-gpu-layers"),
      crate::launch::knobs::Scalar::U32(99),
    );
    p.extras = vec!["--rope-freq-base".into(), "10000".into()];
    let json = serde_json::to_string(&p).unwrap();
    let back: LaunchParams = serde_json::from_str(&json).unwrap();
    assert_eq!(back, p);
  }

  #[test]
  fn empty_backend_knobs_is_omitted_from_serialized_shape() {
    // `skip_serializing_if` keeps the persisted shape byte-stable for
    // llama.cpp / Lemonade (neither declares native knobs): no
    // `backend_knobs` key.
    let p = base_params();
    assert!(p.knobs.is_empty());
    let json = serde_json::to_string(&p).unwrap();
    assert!(
      !json.contains("backend_knobs"),
      "empty backend_knobs must not appear in the wire shape, got {json}"
    );
  }
}
