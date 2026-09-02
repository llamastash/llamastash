//! The `fake_llama_server` fixture's `--list-devices` mode, exercised
//! through the real `list_devices` parser. This is how the daemon's
//! launch-device catalog — and the TUI's Multi-GPU placement knobs,
//! which gate on `catalog.len() > 1` — get emulated without real GPUs.
//!
//! See `tests/fixtures/fake_llama_server.rs` and its `FAKE_LLAMA_DEVICES`
//! env knob (`0` = CPU-only / default, `1` = single-GPU, `2`+ = multi).

use std::path::PathBuf;
use std::process::Command;

use llamastash::backend::llama_cpp::parse_list_devices;

fn fixture() -> PathBuf {
  PathBuf::from(env!("CARGO_BIN_EXE_fake_llama_server"))
}

/// Run `<fixture> --list-devices` with an explicit per-process
/// `FAKE_LLAMA_DEVICES` (never mutating this test's own env, so the
/// cases stay parallel-safe) and return its stdout.
fn list_devices_stdout(count: &str) -> String {
  let out = Command::new(fixture())
    .arg("--list-devices")
    .env("FAKE_LLAMA_DEVICES", count)
    .output()
    .expect("spawn fake_llama_server --list-devices");
  assert!(
    out.status.success(),
    "fixture --list-devices exited non-zero: {:?}",
    out.status
  );
  String::from_utf8(out.stdout).expect("utf8 stdout")
}

#[test]
fn defaults_to_cpu_only_when_count_unset() {
  // No FAKE_LLAMA_DEVICES → inert: header only, zero devices. This is
  // what keeps every existing daemon-with-fixture test's catalog empty.
  let out = Command::new(fixture())
    .arg("--list-devices")
    .env_remove("FAKE_LLAMA_DEVICES")
    .output()
    .expect("spawn");
  assert!(out.status.success());
  let devs = parse_list_devices(&String::from_utf8_lossy(&out.stdout));
  assert!(devs.is_empty(), "expected no devices, got {devs:?}");
}

#[test]
fn emulates_multi_gpu_host() {
  let devs = parse_list_devices(&list_devices_stdout("2"));
  assert_eq!(devs.len(), 2);
  assert_eq!(devs[0].selector, "Vulkan0");
  assert_eq!(devs[1].selector, "Vulkan1");
  assert_eq!(devs[0].backend, "Vulkan");
  assert!(devs[0].total_mib.is_some(), "memory parsed: {:?}", devs[0]);
  // Distinct adapters in one compute family, so these are two cards, not one
  // card seen twice — the TUI's `multi_device()` gate opens.
  assert!(devs.len() > 1);
  assert_ne!(devs[0].name, devs[1].name);
}

#[test]
fn emulates_single_gpu_host() {
  let devs = parse_list_devices(&list_devices_stdout("1"));
  assert_eq!(devs.len(), 1, "single GPU → multi_device() is false");
  assert_eq!(devs[0].selector, "Vulkan0");
}
