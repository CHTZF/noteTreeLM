use crate::app_state::ApiState;

pub(crate) async fn run(
    state: ApiState,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) {
    tracing::info!("Scheduler started");
    // Track last cleanup to run once per day
    let mut last_cleanup_ts: i64 = 0;
    loop {
        // Sleep for 60s but wake immediately on shutdown signal
        tokio::select! {
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(60)) => {}
            _ = shutdown_rx.recv() => {
                tracing::info!("Scheduler received shutdown signal — exiting");
                break;
            }
        }

        let now_ts = chrono::Utc::now().timestamp();

        // Daily cleanup: delete expired memory_facts (runs at most once per 24h)
        if now_ts - last_cleanup_ts >= 86400 {
            match state.db
                .query("DELETE memory_facts WHERE expires_at < $now")
                .bind(("now", now_ts))
                .await
            {
                Ok(_) => tracing::info!("[scheduler] expired memory_facts cleanup done"),
                Err(e) => tracing::warn!("[scheduler] cleanup error: {}", e),
            }
            last_cleanup_ts = now_ts;
        }

        #[derive(serde::Deserialize)]
        struct TaskRow {
            task_id: String,
            vault_id: String,
            account_id: Option<String>,
            description: String,
            agent_def_name: Option<String>,
            agent_prompt: Option<String>,
            repeat_interval_secs: i64,
        }

        let mut resp = match state.db.query(
            "SELECT task_id, vault_id, account_id, description, agent_def_name, agent_prompt, repeat_interval_secs \
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
                "Scheduled task due: {} (vault={}, agent={:?})",
                task.description, task.vault_id, task.agent_def_name
            );

            // Emit SSE notification to frontend
            state.daemon.emit("schedule:triggered", serde_json::json!({
                "task_id": task.task_id,
                "vault_id": task.vault_id,
                "description": task.description,
            }));

            // Execute agent in background
            let state_clone = state.clone();
            let tid = task.task_id.clone();
            let vid = task.vault_id.clone();
            let aid = task.account_id.clone().unwrap_or_default();
            let desc = task.description.clone();
            let agent_name = task.agent_def_name.clone();
            let agent_prompt = task.agent_prompt.clone();
            tokio::spawn(async move {
                crate::service::execute_scheduled_task(
                    state_clone, tid, vid, aid, agent_name, agent_prompt, desc,
                ).await;
            });

            // Update schedule
            if task.repeat_interval_secs > 0 {
                let next_ts = now_ts + task.repeat_interval_secs;
                let _ = state.db.query(
                    "UPDATE scheduled_tasks SET run_at_ts = $next WHERE task_id = $tid"
                )
                .bind(("next", next_ts))
                .bind(("tid", task.task_id.clone()))
                .await;
            } else {
                let _ = state.db.query(
                    "UPDATE scheduled_tasks SET status = 'done' WHERE task_id = $tid"
                )
                .bind(("tid", task.task_id.clone()))
                .await;
            }
        }
    }
}
