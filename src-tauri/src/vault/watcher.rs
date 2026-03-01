use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

/// 啟動 FileWatcher，回傳停止 sender。
/// Drop 該 sender（或呼叫 stop_watcher）即可停止舊的 watcher thread。
pub fn start_watcher(app: AppHandle, vault_path: PathBuf) -> mpsc::SyncSender<()> {
    let (stop_tx, stop_rx) = mpsc::sync_channel::<()>(1);

    std::thread::spawn(move || {
        let (tx, rx) = mpsc::channel::<notify::Result<Event>>();

        let mut watcher = match RecommendedWatcher::new(
            move |res| {
                let _ = tx.send(res);
            },
            Config::default().with_poll_interval(Duration::from_millis(500)),
        ) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("FileWatcher 初始化失敗：{}", e);
                return;
            }
        };

        if let Err(e) = watcher.watch(&vault_path, RecursiveMode::Recursive) {
            eprintln!("FileWatcher 監控失敗：{}", e);
            return;
        }

        loop {
            // 優先檢查停止信號（非阻塞）
            if stop_rx.try_recv().is_ok() {
                break;
            }
            // 等待 notify 事件（最多 200ms，避免阻塞停止檢查）
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Ok(event)) => handle_event(&app, event),
                Ok(Err(e)) => eprintln!("FileWatcher 事件錯誤：{}", e),
                Err(mpsc::RecvTimeoutError::Timeout) => {} // 繼續 loop
                Err(mpsc::RecvTimeoutError::Disconnected) => break, // sender dropped
            }
        }
    });

    stop_tx
}

fn handle_event(app: &AppHandle, event: Event) {
    use notify::EventKind::*;

    let paths: Vec<String> = event
        .paths
        .iter()
        .filter(|p| {
            // 只處理 .md 檔案，並排除 .trash/ 目錄
            if !p.extension().map(|e| e == "md").unwrap_or(false) {
                return false;
            }
            !p.components().any(|c| {
                c.as_os_str().to_string_lossy().starts_with('.')
            })
        })
        .map(|p| p.to_string_lossy().to_string())
        .collect();

    if paths.is_empty() {
        return;
    }

    match event.kind {
        Create(_) => {
            let _ = app.emit("vault:note-created", &paths);
        }
        Modify(_) => {
            let _ = app.emit("vault:note-updated", &paths);
        }
        Remove(_) => {
            let _ = app.emit("vault:note-deleted", &paths);
        }
        _ => {}
    }
}
