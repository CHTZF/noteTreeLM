use crate::app_state::ApiState;

pub(crate) async fn run(
    state: ApiState,
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) {
    tracing::info!("Scheduler started");
    let mut last_cleanup_ts: i64 = 0;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(60)) => {}
            _ = shutdown_rx.recv() => {
                tracing::info!("Scheduler received shutdown signal — exiting");
                break;
            }
        }

        let now_ts = chrono::Utc::now().timestamp();

        if now_ts - last_cleanup_ts >= 86400 {
            match state.db
                .query("DELETE memory_facts WHERE expires_at < $now")
                .bind(("now", now_ts))
                .await
            {
                Ok(_)  => tracing::info!("[scheduler] expired memory_facts cleanup done"),
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
            Ok(r)  => r,
            Err(e) => { tracing::error!("Scheduler query error: {}", e); continue; }
        };

        let due: Vec<TaskRow> = resp.take(0).unwrap_or_default();
        for task in due {
            tracing::info!(
                "Scheduled task due: {} (vault={}, agent={:?})",
                task.description, task.vault_id, task.agent_def_name
            );

            state.daemon.emit("schedule:triggered", serde_json::json!({
                "task_id":     task.task_id,
                "vault_id":    task.vault_id,
                "description": task.description,
            }));

            // Only spawn agent tasks that have an agent configured.
            let agent_name = match task.agent_def_name.as_deref() {
                Some(n) if !n.is_empty() => n.to_string(),
                _ => continue,
            };

            let state_c   = state.clone();
            let tid       = task.task_id.clone();
            let vid       = task.vault_id.clone();
            let aid       = task.account_id.clone().unwrap_or_default();
            let desc      = task.description.clone();
            let prompt    = task.agent_prompt.clone();

            tokio::spawn(async move {
                // Resolve account_id if missing (old rows without account_id).
                let account_id = if aid.is_empty() {
                    #[derive(serde::Deserialize)]
                    struct AccRow { account_id: String }
                    state_c.db
                        .query("SELECT account_id FROM vaults WHERE vault_id = $vid LIMIT 1")
                        .bind(("vid", vid.clone()))
                        .await.ok()
                        .and_then(|mut r| r.take::<Vec<AccRow>>(0).ok())
                        .and_then(|rows| rows.into_iter().next())
                        .map(|r| r.account_id)
                        .unwrap_or_default()
                } else { aid };

                let agent_def = match crate::service::helpers::load_agent_def(
                    &state_c.db, &agent_name, &account_id,
                ).await {
                    Some(a) => a,
                    None => {
                        tracing::warn!(
                            "[scheduler] agent '{}' not found for account '{}'",
                            agent_name, account_id
                        );
                        return;
                    }
                };

                let initial_msg  = prompt.unwrap_or_else(|| desc.clone());
                let conversation_id = format!(
                    "scheduled_{}_{}_{}", tid, agent_name, chrono::Utc::now().timestamp()
                );

                let runtime = match crate::service::build_agent_runtime(
                    &state_c, &vid, &account_id,
                    None, conversation_id, agent_def,
                    false, // background — no llm:token SSE
                    None,  // no ui_language for scheduled tasks
                ).await {
                    Some(r) => r,
                    None => {
                        tracing::warn!("[scheduler] llm_url not available, skipping task {}", tid);
                        return;
                    }
                };

                crate::service::execute_scheduled_task(runtime, tid, desc, initial_msg).await;
            });

            if task.repeat_interval_secs > 0 {
                let next_ts = now_ts + task.repeat_interval_secs;
                let _ = state.db
                    .query("UPDATE scheduled_tasks SET run_at_ts = $next WHERE task_id = $tid")
                    .bind(("next", next_ts))
                    .bind(("tid", task.task_id.clone()))
                    .await;
            } else {
                let _ = state.db
                    .query("UPDATE scheduled_tasks SET status = 'done' WHERE task_id = $tid")
                    .bind(("tid", task.task_id.clone()))
                    .await;
            }
        }
    }
}
