//! Resolve every file a "delete this model" must remove, then remove it.
//!
//! A catalog row is rarely one file on disk. A split set carries shards
//! 2..N, a multimodal model carries an `mmproj-*.gguf` projector, an
//! MTP-capable model can carry a separate `mtp-*.gguf` draft head, and a
//! HuggingFace snapshot entry is a symlink whose bytes live in a sibling
//! `blobs/` file. Unlinking just the launch path leaves the rest behind and
//! the freed space never shows up.
//!
//! The plan is resolved before the confirm prompt so the popup can say what
//! is about to go and the confirm handler does no fresh discovery.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::discovery::{scanner, DiscoveredModel};

/// Files a confirmed delete will remove.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeletePlan {
  /// The row's own file — shard 1 of a split set, else the model itself.
  pub primary: PathBuf,
  /// Shards 2..N of a split set. Empty for a single-file model.
  pub shards: Vec<PathBuf>,
  /// The mmproj projector companion, when no other model in the folder
  /// pairs with it.
  pub projector: Option<PathBuf>,
  /// The separate MTP draft head, under the same exclusivity rule.
  pub mtp_head: Option<PathBuf>,
  /// HuggingFace cache repo directory to remove wholesale, blobs and all.
  /// Set only when this row is the last model in that repo — with a second
  /// quant still there the per-file path runs instead so the survivor keeps
  /// its bytes.
  pub hf_repo_dir: Option<PathBuf>,
  /// Resolved HF cache root, carried so [`execute`] can decide whether a
  /// snapshot symlink's blob is ours to unlink.
  cache_root: Option<PathBuf>,
  /// Whether the files sit in the HF cache, so [`DeletePlan::describe`] only
  /// promises reclaimed blobs where there are blobs to reclaim.
  in_hf_cache: bool,
}

impl DeletePlan {
  /// A plan that unlinks one file and nothing else.
  pub fn single(primary: impl Into<PathBuf>) -> Self {
    DeletePlan {
      primary: primary.into(),
      ..DeletePlan::default()
    }
  }

  /// Every file the plan unlinks individually, primary first. Empty when
  /// the plan removes a whole repo directory instead.
  pub fn files(&self) -> Vec<&Path> {
    let mut out = vec![self.primary.as_path()];
    out.extend(self.shards.iter().map(PathBuf::as_path));
    out.extend(self.projector.as_deref());
    out.extend(self.mtp_head.as_deref());
    out
  }

  /// Confirm-popup body: what is going, and why it is more than one file.
  /// One flowing paragraph — the overlay renders a single wrapped `Line`, so
  /// embedded newlines would collapse mid-sentence rather than break.
  pub fn describe(&self, display_name: &str) -> String {
    let head = format!("Delete `{display_name}` from disk?");
    if let Some(repo) = &self.hf_repo_dir {
      let repo_name = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo.display().to_string());
      return format!(
        "{head} It is the last model in the HuggingFace cache repo \
         `{repo_name}`, so the whole directory goes — every revision, shard \
         and blob."
      );
    }
    let mut extras: Vec<String> = Vec::new();
    if !self.shards.is_empty() {
      extras.push(format!(
        "{} more shard{}",
        self.shards.len(),
        if self.shards.len() == 1 { "" } else { "s" }
      ));
    }
    if self.projector.is_some() {
      extras.push("its mmproj projector".to_string());
    }
    if self.mtp_head.is_some() {
      extras.push("its MTP draft head".to_string());
    }
    let blobs = if self.in_hf_cache {
      " The HuggingFace cache blobs behind them are freed too."
    } else {
      ""
    };
    match extras.len() {
      // A whole-directory row removes a tree, not a file. Saying "one file"
      // there understates an irreversible action on the one shape where the
      // user is least able to guess what it covers.
      0 if self.primary.is_dir() => {
        format!("{head} The whole model directory goes — every file inside it.{blobs}")
      }
      0 => format!("{head} One file is unlinked.{blobs}"),
      _ => format!(
        "{head} {} files go: the model plus {}.{blobs}",
        self.files().len(),
        join_human(&extras)
      ),
    }
  }
}

/// Build the delete plan for `target`.
///
/// `catalog` is the live model list, used for two exclusivity questions a
/// lone path cannot answer: does another model in the same folder also pair
/// with this companion, and is another model still living in this HF repo.
pub fn plan(
  target: &DiscoveredModel,
  catalog: &[DiscoveredModel],
  cache_root: Option<&Path>,
) -> DeletePlan {
  let mut plan = DeletePlan {
    primary: target.path.clone(),
    shards: target.split_siblings.clone(),
    cache_root: cache_root.map(Path::to_path_buf),
    ..DeletePlan::default()
  };

  let neighbours: Vec<&DiscoveredModel> = catalog
    .iter()
    .filter(|m| m.path != target.path && m.parent == target.parent)
    .collect();

  // Companions are a GGUF-file notion. A whole-directory row (a safetensors
  // snapshot) owns none, and the finders would walk the enclosing `snapshots/`
  // dir for an mmproj that cannot be there.
  if !target.path.is_dir() {
    // Resolved from disk rather than from the catalog's capability flags: a
    // projector whose own header won't parse leaves `multimodal` unset but is
    // still a file this model owns. Both finders are a directory walk, and the
    // per-neighbour re-resolution below only runs once one actually matched.
    plan.projector = scanner::find_mmproj(&target.path).filter(|proj| {
      !neighbours
        .iter()
        .any(|m| scanner::find_mmproj(&m.path).as_deref() == Some(proj.as_path()))
    });
    let arch = target.metadata.as_ref().and_then(|m| m.arch.as_deref());
    plan.mtp_head = scanner::find_mtp_head(&target.path, arch).filter(|head| {
      !neighbours.iter().any(|m| {
        let neighbour_arch = m.metadata.as_ref().and_then(|md| md.arch.as_deref());
        scanner::find_mtp_head(&m.path, neighbour_arch).as_deref() == Some(head.as_path())
      })
    });
  }

  // Whole-repo removal reclaims the non-GGUF cache cruft (refs, configs,
  // stale revisions) that per-file unlinking cannot see, but only once
  // nothing else in the repo is still listed.
  if let Some(repo_dir) = hf_repo_dir_in_cache(&target.path, cache_root) {
    plan.in_hf_cache = true;
    let shape = hf_repo_dir_shape(&target.path);
    let repo_still_populated = catalog
      .iter()
      .any(|m| m.path != target.path && hf_repo_dir_shape(&m.path) == shape);
    if !repo_still_populated {
      plan.hf_repo_dir = Some(repo_dir);
    }
  }

  plan
}

/// [`plan`] against the live HF cache root, resolving `path` to its catalog
/// row. A path with no row (the catalog refreshed underneath the cursor)
/// degrades to a single-file plan rather than refusing.
pub fn plan_for_path(path: &Path, catalog: &[DiscoveredModel]) -> DeletePlan {
  let cache_root = crate::init::download::hf_cache_dir().ok();
  match catalog.iter().find(|m| m.path == path) {
    Some(target) => plan(target, catalog, cache_root.as_deref()),
    None => DeletePlan {
      primary: path.to_path_buf(),
      cache_root,
      ..DeletePlan::default()
    },
  }
}

/// Carry out `plan`, returning a human-readable summary for the toast.
///
/// A missing file is not an error: discovery can lag a delete that happened
/// outside llamastash, and failing the whole operation because a companion
/// was already gone would leave the rest on disk.
pub fn execute(plan: &DeletePlan) -> io::Result<String> {
  if let Some(repo_dir) = &plan.hf_repo_dir {
    fs::remove_dir_all(repo_dir)?;
    return Ok(format!(
      "deleted HF cache for {}",
      repo_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("model")
    ));
  }

  // A directory primary cannot go through `remove_file` (EISDIR, surfaced as
  // an opaque failure). Two ways to get here, and they are not the same
  // situation — saying the wrong one sent users looking for a sibling model
  // that does not exist.
  if plan.primary.is_dir() {
    if plan.in_hf_cache {
      return Err(io::Error::other(format!(
        "{} is not the only model in its cache repo — refusing to delete it",
        plan.primary.display()
      )));
    }
    // Outside the resolved cache root: nothing else shares this directory, so
    // removing it is exactly what the user asked for.
    fs::remove_dir_all(&plan.primary)?;
    return Ok(format!(
      "deleted {}",
      plan
        .primary
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("model")
    ));
  }

  let files = plan.files();
  let mut removed = 0usize;
  let mut first_error = None;
  for file in &files {
    match remove_file_and_blob(file, plan.cache_root.as_deref()) {
      Ok(()) => removed += 1,
      Err(e) if e.kind() == io::ErrorKind::NotFound => {}
      Err(e) => {
        first_error.get_or_insert(e);
      }
    }
  }
  // Only the primary failing is fatal — a companion we could not unlink is
  // reported but the model itself is gone, which is what the user asked for.
  if !plan.primary.exists() {
    let name = plan
      .primary
      .file_name()
      .and_then(|n| n.to_str())
      .unwrap_or("model");
    return Ok(match removed {
      0 | 1 => format!("deleted {name}"),
      n => format!("deleted {name} + {} companion files", n - 1),
    });
  }
  Err(first_error.unwrap_or_else(|| io::Error::other("delete removed nothing")))
}

/// Unlink `file`, plus the HF cache blob it points at when it is a snapshot
/// symlink into its own repo's `blobs/` directory. A link pointing anywhere
/// else is unlinked without touching its target.
fn remove_file_and_blob(file: &Path, cache_root: Option<&Path>) -> io::Result<()> {
  let blob = hf_blob_target(file, cache_root);
  fs::remove_file(file)?;
  if let Some(blob) = blob {
    let _ = fs::remove_file(blob);
  }
  Ok(())
}

/// The `blobs/<sha>` file backing an HF snapshot symlink, or `None` when
/// `file` is a plain file, sits outside the cache, or links out of the repo.
fn hf_blob_target(file: &Path, cache_root: Option<&Path>) -> Option<PathBuf> {
  let repo_dir = hf_repo_dir_in_cache(file, cache_root)?;
  if !fs::symlink_metadata(file).ok()?.file_type().is_symlink() {
    return None;
  }
  let link = fs::read_link(file).ok()?;
  let target = if link.is_absolute() {
    link
  } else {
    file.parent()?.join(link)
  };
  let target = target.canonicalize().ok()?;
  target.starts_with(repo_dir.join("blobs")).then_some(target)
}

/// The `models--<owner>--<repo>` directory `path` sits under by directory
/// shape alone, with no cache-root gate and no canonicalisation. Pure path
/// arithmetic, so two paths from the same discovery pass compare reliably —
/// that is all the "is anything else still in this repo" question needs.
///
/// Three row shapes resolve to the same repo: a GGUF **file**
/// (`models--*/snapshots/<rev>/<file>`), the same file nested in a quant
/// **subdirectory** (`.../snapshots/<rev>/Q4_K_M/<file>`, which the recursive
/// GGUF scanner emits as its own row), and a whole-snapshot **directory**
/// (`models--*/snapshots/<rev>`), which is what a safetensors row carries.
/// All must land on the same answer or the "last model in this repo" check
/// would treat a mixed catalog as unrelated repos — and a nested GGUF that
/// resolved to `None` used to be invisible to that check, so deleting the
/// directory row planned a whole-repo `remove_dir_all` straight over it.
fn hf_repo_dir_shape(path: &Path) -> Option<&Path> {
  // Walk up to `snapshots/` from any depth rather than testing fixed levels.
  let mut cursor = path;
  while let Some(parent) = cursor.parent() {
    if parent.file_name().and_then(|n| n.to_str()) == Some("snapshots") {
      let repo_dir = parent.parent()?;
      return repo_dir
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("models--"))
        .then_some(repo_dir);
    }
    cursor = parent;
  }
  None
}

/// [`hf_repo_dir_shape`] plus the gate that makes recursive removal safe: the
/// resolved repo directory must live inside `cache_root`. An HF-shaped tree
/// somewhere else — an rsynced backup, a restored archive, a surprise Docker
/// volume — resolves to `None` and takes the per-file path instead.
fn hf_repo_dir_in_cache(path: &Path, cache_root: Option<&Path>) -> Option<PathBuf> {
  let repo_dir = hf_repo_dir_shape(path)?;
  let cache_root = cache_root?;
  let cache_root_canonical = cache_root
    .canonicalize()
    .unwrap_or_else(|_| cache_root.to_path_buf());
  let candidate = repo_dir
    .canonicalize()
    .unwrap_or_else(|_| repo_dir.to_path_buf());
  candidate
    .starts_with(&cache_root_canonical)
    .then_some(candidate)
}

/// `["a"] -> "a"`, `["a", "b"] -> "a and b"`, `["a", "b", "c"] -> "a, b and c"`.
fn join_human(parts: &[String]) -> String {
  match parts {
    [] => String::new(),
    [one] => one.clone(),
    [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::discovery::ModelSource;
  use crate::gguf::metadata::{ModeHint, ModelMetadata, Quant};

  fn model(path: &Path) -> DiscoveredModel {
    DiscoveredModel {
      path: path.to_path_buf(),
      parent: path.parent().unwrap_or(Path::new("/")).to_path_buf(),
      source: ModelSource::UserPath,
      metadata: None,
      parse_error: None,
      split_siblings: Vec::new(),
      display_label: None,
      multimodal: None,
      supported_backends: vec!["llamacpp".into()],
      mtp_head: None,
    }
  }

  fn with_arch(mut m: DiscoveredModel, arch: &str) -> DiscoveredModel {
    m.metadata = Some(ModelMetadata {
      arch: Some(arch.to_string()),
      total_parameters: None,
      parameter_label: None,
      quant: Quant::Unknown(0),
      quant_label: None,
      native_ctx: None,
      chat_template: None,
      tokenizer_kind: None,
      reasoning_hint: false,
      mode_hint: ModeHint::Unknown,
      weights_bytes: None,
      mtp: None,
    });
    m
  }

  fn tempdir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
      "llamastash-delete-{label}-{}-{:?}",
      std::process::id(),
      std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
  }

  #[test]
  fn plan_takes_every_shard_of_a_split_set() {
    let dir = tempdir("shards");
    let shard1 = dir.join("big-00001-of-00003.gguf");
    let shard2 = dir.join("big-00002-of-00003.gguf");
    let shard3 = dir.join("big-00003-of-00003.gguf");
    for p in [&shard1, &shard2, &shard3] {
      fs::write(p, b"weights").unwrap();
    }
    let mut target = model(&shard1);
    target.split_siblings = vec![shard2.clone(), shard3.clone()];

    let plan = plan(&target, std::slice::from_ref(&target), None);
    assert_eq!(plan.files().len(), 3);
    let summary = execute(&plan).expect("delete must succeed");
    assert!(summary.contains("companion files"), "got `{summary}`");
    for p in [&shard1, &shard2, &shard3] {
      assert!(!p.exists(), "{} must be gone", p.display());
    }
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn plan_takes_projector_and_mtp_head_of_a_lone_model() {
    // The catalog row carries no capability flags here on purpose: these
    // companion files have unparseable headers, so discovery left
    // `multimodal` / `mtp_head` unset. They are still this model's files.
    let dir = tempdir("companions");
    let gguf = dir.join("gemma-4-Q4_K_M.gguf");
    let proj = dir.join("mmproj-gemma-4-f16.gguf");
    let head = dir.join("mtp-gemma-4.gguf");
    for p in [&gguf, &proj, &head] {
      fs::write(p, b"weights").unwrap();
    }
    let target = model(&gguf);

    let plan = plan(&target, std::slice::from_ref(&target), None);
    assert_eq!(plan.projector.as_deref(), Some(proj.as_path()));
    assert_eq!(plan.mtp_head.as_deref(), Some(head.as_path()));
    execute(&plan).expect("delete must succeed");
    for p in [&gguf, &proj, &head] {
      assert!(!p.exists(), "{} must be gone", p.display());
    }
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn plan_keeps_a_companion_another_model_still_pairs_with() {
    // Two models sharing one anonymous projector: `find_mmproj`'s catch-all
    // rule hands the same file to both, so deleting one must leave it.
    let dir = tempdir("shared-proj");
    let a = dir.join("alpha-Q4_K_M.gguf");
    let b = dir.join("beta-Q4_K_M.gguf");
    let proj = dir.join("mmproj.gguf");
    for p in [&a, &b, &proj] {
      fs::write(p, b"weights").unwrap();
    }
    let target = model(&a);
    let catalog = vec![target.clone(), model(&b)];

    let plan = plan(&target, &catalog, None);
    assert_eq!(plan.projector, None, "shared projector must survive");
    execute(&plan).expect("delete must succeed");
    assert!(!a.exists());
    assert!(
      proj.exists(),
      "the surviving model still needs its projector"
    );
    assert!(b.exists());
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn plan_removes_whole_hf_repo_when_it_holds_the_last_model() {
    let cache_root = tempdir("hf-last");
    let repo_dir = cache_root.join("models--owner--repo");
    let blobs = repo_dir.join("blobs");
    let snap = repo_dir.join("snapshots").join("main");
    fs::create_dir_all(&blobs).unwrap();
    fs::create_dir_all(&snap).unwrap();
    fs::write(blobs.join("sha"), b"blob").unwrap();
    let file = snap.join("file.gguf");
    symlink_or_copy(&blobs.join("sha"), &file);

    let target = model(&file);
    let plan = plan(&target, std::slice::from_ref(&target), Some(&cache_root));
    assert!(plan.hf_repo_dir.is_some());
    let summary = execute(&plan).expect("delete must succeed");
    assert!(summary.contains("HF cache"), "got `{summary}`");
    assert!(!repo_dir.exists(), "the whole repo dir should be gone");
    let _ = fs::remove_dir_all(&cache_root);
  }

  #[test]
  fn plan_spares_a_second_quant_sharing_the_hf_repo() {
    // Two quants pulled from one repo land in the same `models--*` dir.
    // Deleting one must take its blob and leave the other runnable.
    let cache_root = tempdir("hf-two-quants");
    let repo_dir = cache_root.join("models--owner--repo");
    let blobs = repo_dir.join("blobs");
    let snap = repo_dir.join("snapshots").join("main");
    fs::create_dir_all(&blobs).unwrap();
    fs::create_dir_all(&snap).unwrap();
    fs::write(blobs.join("sha-q4"), b"q4").unwrap();
    fs::write(blobs.join("sha-q8"), b"q8").unwrap();
    let q4 = snap.join("model-Q4_K_M.gguf");
    let q8 = snap.join("model-Q8_0.gguf");
    symlink_or_copy(&blobs.join("sha-q4"), &q4);
    symlink_or_copy(&blobs.join("sha-q8"), &q8);

    let target = model(&q4);
    let catalog = vec![target.clone(), model(&q8)];
    let plan = plan(&target, &catalog, Some(&cache_root));
    assert_eq!(
      plan.hf_repo_dir, None,
      "a repo with a second model must not be nuked wholesale"
    );
    execute(&plan).expect("delete must succeed");
    assert!(!q4.exists(), "the deleted snapshot link must be gone");
    assert!(q8.exists(), "the other quant must survive");
    assert!(blobs.join("sha-q8").exists(), "its blob must survive too");
    #[cfg(unix)]
    assert!(
      !blobs.join("sha-q4").exists(),
      "the deleted model's blob must be reclaimed"
    );
    let _ = fs::remove_dir_all(&cache_root);
  }

  #[test]
  fn hf_shaped_tree_outside_the_cache_root_only_unlinks() {
    let outside = tempdir("hf-outside");
    let repo_dir = outside.join("models--owner--repo");
    let snap = repo_dir.join("snapshots").join("main");
    fs::create_dir_all(&snap).unwrap();
    let file = snap.join("file.gguf");
    let other = snap.join("other.gguf");
    fs::write(&file, b"weights").unwrap();
    fs::write(&other, b"other").unwrap();
    let unrelated_cache = tempdir("hf-unrelated");

    let target = model(&file);
    let plan = plan(
      &target,
      std::slice::from_ref(&target),
      Some(&unrelated_cache),
    );
    assert_eq!(plan.hf_repo_dir, None);
    let summary = execute(&plan).expect("delete must succeed");
    assert!(!summary.contains("HF cache"), "got `{summary}`");
    assert!(!file.exists());
    assert!(other.exists(), "sibling file must not be removed");
    assert!(repo_dir.exists(), "the repo dir must not be removed");
    let _ = fs::remove_dir_all(&outside);
    let _ = fs::remove_dir_all(&unrelated_cache);
  }

  #[test]
  fn no_cache_root_never_recurses() {
    let dir = tempdir("no-cache-root");
    let repo_dir = dir.join("models--owner--repo");
    let snap = repo_dir.join("snapshots").join("main");
    fs::create_dir_all(&snap).unwrap();
    let file = snap.join("file.gguf");
    fs::write(&file, b"weights").unwrap();

    let target = model(&file);
    let plan = plan(&target, std::slice::from_ref(&target), None);
    assert_eq!(plan.hf_repo_dir, None);
    execute(&plan).expect("delete must succeed");
    assert!(!file.exists());
    assert!(repo_dir.exists());
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn missing_companion_does_not_fail_the_delete() {
    let dir = tempdir("missing-companion");
    let gguf = dir.join("m.gguf");
    fs::write(&gguf, b"weights").unwrap();
    let mut target = model(&gguf);
    target.split_siblings = vec![dir.join("m-vanished.gguf")];

    let plan = plan(&target, std::slice::from_ref(&target), None);
    let summary = execute(&plan).expect("a vanished sibling must not fail the delete");
    assert!(summary.contains("m.gguf"), "got `{summary}`");
    assert!(!gguf.exists());
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn arch_matched_mtp_head_is_kept_when_a_sibling_model_also_drafts_from_it() {
    // Both models declare `deepseek4`, so the head's `deepseek4_mtp_support`
    // arch pairs with either. Deleting one must leave the head for the other.
    use crate::gguf::test_fixtures::FixtureBuilder;
    let dir = tempdir("shared-head");
    let a = dir.join("ds4-flash-Q2_K.gguf");
    let b = dir.join("ds4-pro-Q2_K.gguf");
    let head = dir.join("mtp-deepseek-v4.gguf");
    fs::write(&a, b"weights").unwrap();
    fs::write(&b, b"weights").unwrap();
    fs::write(
      &head,
      FixtureBuilder::new()
        .with_arch("deepseek4_mtp_support")
        .build(),
    )
    .unwrap();

    let target = with_arch(model(&a), "deepseek4");
    let catalog = vec![target.clone(), with_arch(model(&b), "deepseek4")];

    let plan = plan(&target, &catalog, None);
    assert_eq!(plan.mtp_head, None, "shared draft head must survive");
    execute(&plan).expect("delete must succeed");
    assert!(head.exists());
    let _ = fs::remove_dir_all(&dir);
  }

  #[test]
  fn describe_names_the_companions_going_with_the_model() {
    let plan = DeletePlan {
      primary: PathBuf::from("/m/a-00001-of-00002.gguf"),
      shards: vec![PathBuf::from("/m/a-00002-of-00002.gguf")],
      projector: Some(PathBuf::from("/m/mmproj-a.gguf")),
      mtp_head: Some(PathBuf::from("/m/mtp-a.gguf")),
      ..DeletePlan::default()
    };
    let body = plan.describe("a");
    assert!(body.contains("4 files"), "got `{body}`");
    assert!(body.contains("1 more shard"), "got `{body}`");
    assert!(body.contains("mmproj projector"), "got `{body}`");
    assert!(body.contains("MTP draft head"), "got `{body}`");
    assert!(
      !body.contains('\n'),
      "the overlay renders one wrapped Line; a newline collapses mid-sentence"
    );
    assert!(
      !body.contains("blobs"),
      "a plain user path has no cache blobs to promise: `{body}`"
    );
  }

  #[test]
  fn describe_single_file_stays_short() {
    let plan = DeletePlan::single("/m/a.gguf");
    let body = plan.describe("a");
    assert!(body.contains("One file"), "got `{body}`");
  }

  #[test]
  fn describe_mentions_blobs_only_inside_the_hf_cache() {
    let cache_root = tempdir("describe-blobs");
    let snap = cache_root.join("models--owner--repo/snapshots/main");
    fs::create_dir_all(&snap).unwrap();
    let a = snap.join("a.gguf");
    let b = snap.join("b.gguf");
    fs::write(&a, b"a").unwrap();
    fs::write(&b, b"b").unwrap();
    let target = model(&a);
    // A second model in the repo keeps this on the per-file path, which is
    // the branch whose copy mentions blobs.
    let catalog = vec![target.clone(), model(&b)];
    let body = plan(&target, &catalog, Some(&cache_root)).describe("a");
    assert!(body.contains("blobs"), "got `{body}`");
    let _ = fs::remove_dir_all(&cache_root);
  }

  #[test]
  fn describe_whole_repo_names_the_repo() {
    let plan = DeletePlan {
      primary: PathBuf::from("/c/models--owner--repo/snapshots/main/a.gguf"),
      hf_repo_dir: Some(PathBuf::from("/c/models--owner--repo")),
      ..DeletePlan::default()
    };
    let body = plan.describe("a");
    assert!(body.contains("models--owner--repo"), "got `{body}`");
  }

  /// A catalog row whose `path` is a whole snapshot directory rather than a
  /// single file — the shape a safetensors backend produces. These four pin
  /// that the file-shaped assumptions in this module degrade safely.
  #[test]
  fn directory_row_resolves_no_companions() {
    let root = tempdir("dir-row-companions");
    let snapshot = root.join("models--o--r/snapshots/rev");
    fs::create_dir_all(&snapshot).unwrap();
    fs::write(snapshot.join("model.safetensors"), b"w").unwrap();
    // An mmproj sitting where the file-shaped finder would look.
    fs::write(root.join("models--o--r/snapshots/mmproj-x.gguf"), b"p").unwrap();

    let target = model(&snapshot);
    let p = plan(&target, std::slice::from_ref(&target), None);
    assert_eq!(p.projector, None, "a directory row owns no projector");
    assert_eq!(p.mtp_head, None, "a directory row owns no MTP head");
    assert!(p.shards.is_empty());
    let _ = fs::remove_dir_all(&root);
  }

  #[test]
  fn directory_row_last_in_repo_plans_whole_repo_removal() {
    let cache_root = tempdir("dir-row-repo");
    let repo = cache_root.join("models--o--r");
    let snapshot = repo.join("snapshots/rev");
    fs::create_dir_all(&snapshot).unwrap();

    let target = model(&snapshot);
    let p = plan(&target, std::slice::from_ref(&target), Some(&cache_root));
    assert_eq!(p.hf_repo_dir.as_deref(), Some(repo.as_path()));
    let _ = fs::remove_dir_all(&cache_root);
  }

  #[test]
  fn directory_row_sharing_a_repo_does_not_plan_repo_removal() {
    let cache_root = tempdir("dir-row-shared-repo");
    let repo = cache_root.join("models--o--r");
    let snapshot = repo.join("snapshots/rev");
    fs::create_dir_all(&snapshot).unwrap();
    let sibling = repo.join("snapshots/rev/other.gguf");

    let target = model(&snapshot);
    let catalog = vec![target.clone(), model(&sibling)];
    let p = plan(&target, &catalog, Some(&cache_root));
    assert_eq!(
      p.hf_repo_dir, None,
      "another model still lives in the repo, so the repo must survive"
    );
    let _ = fs::remove_dir_all(&cache_root);
  }

  /// A GGUF nested one level below the snapshot dir is a real catalog row
  /// (the scanner's walk is recursive). It used to resolve to a `None` repo
  /// shape, so the exclusivity check missed it and deleting the safetensors
  /// directory row planned `remove_dir_all` over the whole repo — destroying
  /// a model the user did not select, behind a prompt saying it was the last.
  #[test]
  fn a_nested_gguf_keeps_its_repo_alive() {
    let cache_root = tempdir("dir-row-nested-gguf");
    let repo = cache_root.join("models--o--r");
    let snapshot = repo.join("snapshots/rev");
    fs::create_dir_all(snapshot.join("Q4_K_M")).unwrap();
    let nested = snapshot.join("Q4_K_M/model.gguf");
    fs::write(&nested, b"g").unwrap();

    assert_eq!(
      hf_repo_dir_shape(&nested),
      Some(repo.as_path()),
      "a nested GGUF must resolve to the same repo as its snapshot dir"
    );

    let target = model(&snapshot);
    let catalog = vec![target.clone(), model(&nested)];
    let p = plan(&target, &catalog, Some(&cache_root));
    assert_eq!(
      p.hf_repo_dir, None,
      "the nested GGUF still lives here, so the repo must survive"
    );
    let _ = fs::remove_dir_all(&cache_root);
  }

  /// Outside the resolved cache root nothing else shares the directory, so
  /// the delete is exactly what the user asked for. This used to refuse with
  /// "is not the only model in its cache repo" — a false reason that left the
  /// row with no route to removal at all.
  #[test]
  fn a_directory_outside_the_cache_root_is_deleted() {
    let root = tempdir("dir-row-outside-cache");
    let snapshot = root.join("snapshots/rev");
    fs::create_dir_all(&snapshot).unwrap();
    fs::write(snapshot.join("model.safetensors"), b"w").unwrap();

    let msg = execute(&DeletePlan::single(&snapshot)).expect("must delete");
    assert!(msg.contains("deleted"), "got `{msg}`");
    assert!(!snapshot.exists(), "the directory must be gone");
    let _ = fs::remove_dir_all(&root);
  }

  /// Inside the cache with a sibling still listed, the repo arm is withheld
  /// and the refusal must state *that* reason.
  #[test]
  fn a_shared_cache_repo_directory_is_refused_with_the_real_reason() {
    let cache_root = tempdir("dir-row-shared-refuse");
    let repo = cache_root.join("models--o--r");
    let snapshot = repo.join("snapshots/rev");
    fs::create_dir_all(&snapshot).unwrap();
    fs::write(snapshot.join("model.safetensors"), b"w").unwrap();
    let sibling = repo.join("snapshots/rev2/other.gguf");
    fs::create_dir_all(sibling.parent().unwrap()).unwrap();
    fs::write(&sibling, b"g").unwrap();

    let target = model(&snapshot);
    let catalog = vec![target.clone(), model(&sibling)];
    let p = plan(&target, &catalog, Some(&cache_root));
    assert_eq!(
      p.hf_repo_dir, None,
      "a sibling survives, so no repo removal"
    );

    let err = execute(&p).expect_err("must refuse");
    assert!(
      err
        .to_string()
        .contains("not the only model in its cache repo"),
      "got `{err}`"
    );
    assert!(
      snapshot.exists(),
      "nothing may be removed on the refusal path"
    );
    let _ = fs::remove_dir_all(&cache_root);
  }

  fn symlink_or_copy(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(not(unix))]
    fs::copy(target, link).unwrap();
  }
}
