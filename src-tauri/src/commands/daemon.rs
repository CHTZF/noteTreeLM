/// Daemon management commands for LaunchAgent (macOS) install/uninstall + status

use std::path::PathBuf;

const PLIST_LABEL: &str = "com.notetreetlm.service";

fn plist_path() -> PathBuf {
    let home = dirs::home_dir().expect("Cannot find home dir");
    home.join("Library/LaunchAgents")
        .join(format!("{}.plist", PLIST_LABEL))
}

fn daemon_binary_path() -> PathBuf {
    // In production: alongside the Tauri main binary in Contents/MacOS/
    // In dev: use CARGO_TARGET_DIR or fallback
    let exe = std::env::current_exe().unwrap_or_default();
    exe.parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("notetreetlm-service")
}

fn build_plist(binary_path: &std::path::Path) -> String {
    let log_dir = dirs::home_dir()
        .map(|h| h.join("Library/Logs/noteTreeLM"))
        .unwrap_or_else(|| PathBuf::from("/tmp"));

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>Program</key>
    <string>{binary}</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log_dir}/service.log</string>
    <key>StandardErrorPath</key>
    <string>{log_dir}/service.err</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_LOG</key>
        <string>info</string>
    </dict>
</dict>
</plist>"#,
        label = PLIST_LABEL,
        binary = binary_path.display(),
        log_dir = log_dir.display(),
    )
}

#[tauri::command]
pub async fn install_daemon_service() -> Result<String, String> {
    #[cfg(not(target_os = "macos"))]
    return Err("LaunchAgent install is only supported on macOS".into());

    #[cfg(target_os = "macos")]
    {
        let binary_path = daemon_binary_path();
        if !binary_path.exists() {
            return Err(format!(
                "Daemon binary not found at: {}",
                binary_path.display()
            ));
        }

        let plist_path = plist_path();
        if let Some(parent) = plist_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }

        // Create log dir
        if let Some(home) = dirs::home_dir() {
            let log_dir = home.join("Library/Logs/noteTreeLM");
            std::fs::create_dir_all(&log_dir).ok();
        }

        let plist_content = build_plist(&binary_path);
        std::fs::write(&plist_path, plist_content).map_err(|e| e.to_string())?;

        // Load the LaunchAgent
        let output = std::process::Command::new("launchctl")
            .args(["load", "-w", &plist_path.to_string_lossy()])
            .output()
            .map_err(|e| e.to_string())?;

        if output.status.success() {
            Ok("Daemon service installed and started".into())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!("launchctl load failed: {}", stderr))
        }
    }
}

#[tauri::command]
pub async fn uninstall_daemon_service() -> Result<String, String> {
    #[cfg(not(target_os = "macos"))]
    return Err("LaunchAgent uninstall is only supported on macOS".into());

    #[cfg(target_os = "macos")]
    {
        let plist_path = plist_path();

        // Unload first (ignore error if not loaded)
        let _ = std::process::Command::new("launchctl")
            .args(["unload", "-w", &plist_path.to_string_lossy()])
            .output();

        // Remove plist
        if plist_path.exists() {
            std::fs::remove_file(&plist_path).map_err(|e| e.to_string())?;
        }

        Ok("Daemon service stopped and uninstalled".into())
    }
}

#[tauri::command]
pub async fn get_daemon_status() -> serde_json::Value {
    let client = match reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return serde_json::json!({ "running": false, "error": "client build failed" }),
    };

    match client
        .get("https://127.0.0.1:7788/health")
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            serde_json::json!({ "running": true, "info": body })
        }
        Ok(resp) => serde_json::json!({ "running": false, "status": resp.status().as_u16() }),
        Err(e) => serde_json::json!({ "running": false, "error": e.to_string() }),
    }
}

#[tauri::command]
pub async fn get_daemon_servers() -> serde_json::Value {
    let client = match reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return serde_json::json!({ "servers": [] }),
    };

    match client
        .get("https://127.0.0.1:7788/servers/status")
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            resp.json().await.unwrap_or(serde_json::json!({ "servers": [] }))
        }
        _ => serde_json::json!({ "servers": [] }),
    }
}
