#![cfg(unix)]

use serde_json::Value;
use std::process::Command;

#[test]
fn status_client_json_advertises_resume_bridge_capability() {
    let root = std::env::temp_dir().join(format!("mosh-status-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_herdr"))
        .args(["status", "client", "--json"])
        .env("HOME", &root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_RUNTIME_DIR", &root)
        .env_remove("HERDR_SOCKET_PATH")
        .env_remove("HERDR_CLIENT_SOCKET_PATH")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&root);
    assert!(output.status.success(), "{output:?}");
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    let caps: Vec<&str> = status["endpoint_capabilities"]
        .as_array()
        .expect("endpoint_capabilities array")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(caps.contains(&"x-mosh-resume-bridge-v1"), "{caps:?}");
    assert_eq!(caps.len(), 4, "three upstream entries plus ours: {caps:?}");
}
