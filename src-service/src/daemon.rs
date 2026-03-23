use std::path::PathBuf;
use tokio::signal;
use crate::auth::store::AuthStore;
use crate::db::SurrealDb;
use crate::state::DaemonState;

pub async fn run(data_dir: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    tracing::info!("noteTreeLM Service starting...");

    let db = crate::db::init_db(&data_dir).await?;
    tracing::info!("Database initialized");

    let hostnames = crate::tls::collect_san_hostnames();
    let tls = crate::tls::generate_tls_cert(hostnames)?;
    tracing::info!("TLS SPKI pin: {}", tls.spki_pin);

    let _mdns = crate::mdns::start_mdns_broadcast(&tls.spki_pin)?;

    let auth_store = AuthStore::new();
    let daemon_state = DaemonState::new();

    // Spawn scheduler loop
    {
        let sched_db = db.clone();
        tokio::spawn(async move {
            run_scheduler(sched_db).await;
        });
    }

    let cert_pem = tls.cert_pem.clone();
    let key_pem = tls.key_pem.clone();
    let store_clone = auth_store.clone();
    let db_clone = db.clone();
    let daemon_state_clone = daemon_state.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::server::run_https_server(
            cert_pem, key_pem, 7788, store_clone, db_clone, daemon_state_clone,
        ).await {
            tracing::error!("HTTPS server error: {}", e);
        }
    });

    tracing::info!("Service ready on https://0.0.0.0:7788");
    shutdown_signal().await;
    tracing::info!("Service shutting down...");
    Ok(())
}

async fn run_scheduler(db: SurrealDb) {
    tracing::info!("Scheduler started");
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;

        let now_ts = chrono::Utc::now().timestamp();

        #[derive(serde::Deserialize)]
        struct TaskRow {
            task_id: String,
            vault_id: String,
            description: String,
            agent_type: Option<String>,
            agent_prompt: Option<String>,
            repeat_interval_secs: i64,
        }

        let mut resp = match db.query(
            "SELECT task_id, vault_id, description, agent_type, agent_prompt, repeat_interval_secs \
             FROM scheduled_tasks \
             WHERE status = 'pending' AND run_at_ts <= $now"
        )
        .bind(("now", now_ts))
        .await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Scheduler query error: {}", e);
                continue;
            }
        };

        let due: Vec<TaskRow> = resp.take(0).unwrap_or_default();
        for task in due {
            tracing::info!(
                "Scheduled task due: {} (vault={}, type={:?})",
                task.description, task.vault_id, task.agent_type
            );

            // Update task state
            if task.repeat_interval_secs > 0 {
                let next_ts = now_ts + task.repeat_interval_secs;
                let _ = db.query(
                    "UPDATE scheduled_tasks SET run_at_ts = $next WHERE task_id = $tid"
                )
                .bind(("next", next_ts))
                .bind(("tid", task.task_id.clone()))
                .await;
            } else {
                let _ = db.query(
                    "UPDATE scheduled_tasks SET status = 'done' WHERE task_id = $tid"
                )
                .bind(("tid", task.task_id.clone()))
                .await;
            }
        }
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c().await.expect("Ctrl+C handler failed");
    };
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler failed")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
