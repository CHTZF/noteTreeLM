use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;
use tauri::{AppHandle, Emitter};

pub fn start_watcher(app: AppHandle, vault_path: PathBuf) {
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

        for result in rx {
            match result {
                Ok(event) => {
                    handle_event(&app, event);
                }
                Err(e) => {
                    eprintln!("FileWatcher 事件錯誤：{}", e);
                }
            }
        }
    });
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
